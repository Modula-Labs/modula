//! Public hostnames. Generated domains live under the instance's apps domain
//! (`MODULA_APPS_DOMAIN`); custom domains with ownership checks come later.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

const MAX_GENERATED: i64 = 5;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Domain {
    pub id: Id,
    pub hostname: String,
    pub kind: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Domain>>> {
    Ok(Json(
        sqlx::query_as(
            "SELECT id, hostname, kind, created_at FROM domains WHERE service_instance_id = $1 ORDER BY created_at",
        )
        .bind(grant.id())
        .fetch_all(&state.db)
        .await?,
    ))
}

/// `<service>-<environment>-<4 random>` (production omits the environment),
/// trimmed so the label stays within 63 characters.
fn generated_label(service: &str, environment: &str) -> anyhow::Result<String> {
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let rand = modula_crypto::random_bytes::<4>()?;
    let suffix: String = rand
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect();
    let env: String = environment
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let base = if env == "production" {
        service.to_owned()
    } else {
        format!("{service}-{env}")
    };
    let base: String = base.chars().take(63 - 5).collect();
    Ok(format!("{}-{suffix}", base.trim_matches('-')))
}

/// Nodes serving this instance, whose routes change with its domains.
async fn serving_nodes(tx: &mut sqlx::PgConnection, instance: Id) -> AppResult<Vec<Id>> {
    let nodes: Vec<Option<Id>> = sqlx::query_scalar(
        "SELECT DISTINCT node_id FROM deployments
         WHERE service_instance_id = $1 AND status IN ('active', 'crashed')",
    )
    .bind(instance)
    .fetch_all(&mut *tx)
    .await?;
    let nodes: Vec<Id> = nodes.into_iter().flatten().collect();
    for n in &nodes {
        deploy::bump(&mut *tx, *n).await?;
    }
    Ok(nodes)
}

pub async fn generate(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Domain>> {
    let instance = grant.id();
    let apps = state.cfg.apps_domain.clone().ok_or_else(|| {
        AppError::Conflict("generated domains are off: set MODULA_APPS_DOMAIN".into())
    })?;
    let (service, environment): (String, String) = sqlx::query_as(
        "SELECT s.name, e.name FROM service_instances i
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
    let id = Id::new();
    // A collision on the random suffix is astronomically unlikely; retry once.
    let mut hostname = String::new();
    for _ in 0..2 {
        hostname = format!("{}.{apps}", generated_label(&service, &environment)?);
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
            break;
        }
    }
    let nodes = serving_nodes(&mut tx, instance).await?;
    Entry::new("domain.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("domain", id)
        .metadata(json!({ "hostname": hostname, "serviceInstance": instance }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(Json(
        sqlx::query_as("SELECT id, hostname, kind, created_at FROM domains WHERE id = $1")
            .bind(id)
            .fetch_one(&state.db)
            .await?,
    ))
}

pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<serde_json::Value>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let (instance, hostname): (Id, String) =
        sqlx::query_as("DELETE FROM domains WHERE id = $1 RETURNING service_instance_id, hostname")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    let nodes = serving_nodes(&mut tx, instance).await?;
    Entry::new("domain.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("domain", id)
        .metadata(json!({ "hostname": hostname }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(Json(json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        let l = generated_label("api", "production").unwrap();
        assert!(l.starts_with("api-") && l.len() == 8, "{l}");
        let l = generated_label("api", "Staging EU").unwrap();
        assert!(l.starts_with("api-staging-eu-"), "{l}");
        let long = "a".repeat(63);
        assert!(generated_label(&long, &long).unwrap().len() <= 63);
    }
}
