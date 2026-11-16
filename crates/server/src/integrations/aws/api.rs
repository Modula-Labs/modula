//! Workspace Settings → Integrations → AWS. The access key is verified,
//! sealed with the workspace data key and never returned.

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use modula_ec2::provider::Ec2Provider;
use modula_ec2::{Credentials, MINIMAL_IAM_POLICY};

use super::{KIND, aad, client, encode_credentials, provider};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::provision::caps::{Caps, Usage, usage};
use crate::provision::{
    ProviderError, ProviderLocation, ProviderSshKey, ServerProvider, ServerTypeOffer,
};
use crate::state::AppState;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub connected: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub connected_at: Option<OffsetDateTime>,
    pub connected_by: Option<String>,
    pub caps: Caps,
    pub usage: Usage,
    /// Where to create an access key.
    pub key_url: &'static str,
    /// The minimal IAM policy to attach to its user.
    pub iam_policy: serde_json::Value,
}

fn iam_policy() -> serde_json::Value {
    serde_json::from_str(MINIMAL_IAM_POLICY).expect("the policy is valid JSON")
}

async fn status_of(state: &AppState, workspace: Id) -> AppResult<Status> {
    let row: Option<(OffsetDateTime, serde_json::Value, Option<String>)> = sqlx::query_as(
        "SELECT i.updated_at, i.metadata, u.name FROM integrations i
         LEFT JOIN users u ON u.id = i.created_by
         WHERE i.workspace_id = $1 AND i.kind = 'aws'",
    )
    .bind(workspace)
    .fetch_optional(&state.db)
    .await?;
    let usage = usage(&state.db, workspace, KIND).await?;
    Ok(match row {
        Some((at, metadata, by)) => Status {
            connected: true,
            connected_at: Some(at),
            connected_by: by,
            caps: Caps::from_metadata(&metadata, state.cfg.eur_usd_rate),
            usage,
            key_url: super::KEY_HELP_URL,
            iam_policy: iam_policy(),
        },
        None => Status {
            connected: false,
            connected_at: None,
            connected_by: None,
            caps: Caps::default(),
            usage,
            key_url: super::KEY_HELP_URL,
            iam_policy: iam_policy(),
        },
    })
}

pub async fn status(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Status>> {
    Ok(Json(status_of(&state, grant.id()).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Connect {
    pub access_key_id: String,
    pub secret_access_key: String,
}

fn valid_access_key_id(k: &str) -> bool {
    (16..=128).contains(&k.len()) && k.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn valid_secret(s: &str) -> bool {
    (16..=256).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_graphic())
}

/// Connect, or replace the key. It must work: `DescribeRegions` proves it is
/// valid and may reach EC2, and a one-entry price query that the catalog
/// will work. Writes are reported by the first server creation.
pub async fn connect(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Connect>,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let access_key_id = req.access_key_id.trim().to_owned();
    let secret = SecretString::from(req.secret_access_key.trim().to_owned());
    if !valid_access_key_id(&access_key_id) || !valid_secret(secret.expose_secret()) {
        return Err(AppError::bad_request(
            "paste the access key id and secret access key of an IAM user",
        ));
    }
    let probe = client(
        &state,
        Credentials {
            access_key_id: access_key_id.clone(),
            secret_access_key: secret.clone(),
        },
    )?;
    let checked = match probe.regions().await {
        Ok(_) => probe.check_pricing().await,
        Err(e) => Err(e),
    };
    match checked {
        Ok(()) => {}
        Err(ProviderError::Unauthorized) => {
            return Err(AppError::bad_request("AWS rejected this access key"));
        }
        Err(ProviderError::Forbidden) => {
            return Err(AppError::bad_request(
                "this access key lacks permissions; attach the IAM policy shown here",
            ));
        }
        Err(e) => return Err(e.into()),
    }

    let mut tx = state.db.begin().await?;
    let existing: Option<Id> = sqlx::query_scalar(
        "SELECT id FROM integrations WHERE workspace_id = $1 AND kind = 'aws' FOR UPDATE",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let id = existing.unwrap_or_default();
    let row = id.to_string();
    let sealed = state
        .keys
        .seal(
            &state.db,
            &encode_credentials(&access_key_id, &secret),
            aad(workspace, &row),
        )
        .await?;
    sqlx::query(
        "INSERT INTO integrations (id, workspace_id, kind, secret_sealed, metadata, created_by)
         VALUES ($1, $2, 'aws', $3, $4, $5)
         ON CONFLICT (id) DO UPDATE SET secret_sealed = EXCLUDED.secret_sealed,
             created_by = EXCLUDED.created_by, updated_at = now()",
    )
    .bind(id)
    .bind(workspace)
    .bind(&sealed)
    .bind(Caps::default().to_metadata())
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    Entry::new("integration.aws.connect", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("integration", id)
        .metadata(json!({ "replaced": existing.is_some() }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(status_of(&state, workspace).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct UpdateCaps {
    pub max_servers: Option<i64>,
    pub max_monthly_cents: Option<i64>,
}

pub async fn update_caps(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateCaps>,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let mut tx = state.db.begin().await?;
    let row: Option<(Id, serde_json::Value)> = sqlx::query_as(
        "SELECT id, metadata FROM integrations WHERE workspace_id = $1 AND kind = 'aws' FOR UPDATE",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((id, metadata)) = row else {
        return Err(AppError::Conflict("connect AWS first".into()));
    };
    let mut caps = Caps::from_metadata(&metadata, state.cfg.eur_usd_rate);
    if let Some(n) = req.max_servers {
        if !(0..=Caps::MAX_SERVERS).contains(&n) {
            return Err(AppError::bad_request(format!(
                "max servers must be 0–{}",
                Caps::MAX_SERVERS
            )));
        }
        caps.max_servers = n;
    }
    if let Some(c) = req.max_monthly_cents {
        if !(0..=Caps::MAX_MONTHLY_CENTS).contains(&c) {
            return Err(AppError::bad_request("monthly limit is out of range"));
        }
        caps.max_monthly_cents = c;
    }
    sqlx::query(
        "UPDATE integrations SET metadata = metadata || $2, updated_at = now() WHERE id = $1",
    )
    .bind(id)
    .bind(caps.to_metadata())
    .execute(&mut *tx)
    .await?;
    Entry::new("integration.aws.caps", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("integration", id)
        .metadata(
            json!({ "maxServers": caps.max_servers, "maxMonthlyCents": caps.max_monthly_cents }),
        )
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(status_of(&state, workspace).await?))
}

/// Refused while servers created through it exist: without the key they
/// could not be deleted from here.
pub async fn disconnect(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Status>> {
    let workspace = grant.id();
    let mut tx = state.db.begin().await?;
    let remaining: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM provisioned_servers
         WHERE workspace_id = $1 AND provider = 'aws' AND provider_id IS NOT NULL
           AND (origin = 'provisioned' OR status = 'resizing')",
    )
    .bind(workspace)
    .fetch_one(&mut *tx)
    .await?;
    if remaining > 0 {
        return Err(AppError::Conflict(format!(
            "delete the {remaining} server(s) created through AWS first"
        )));
    }
    let id: Option<Id> = sqlx::query_scalar(
        "DELETE FROM integrations WHERE workspace_id = $1 AND kind = 'aws' RETURNING id",
    )
    .bind(workspace)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(id) = id else {
        return Err(AppError::NotFound);
    };
    // Rows that never reached AWS have nothing left to clean up, and
    // adopted servers are only links (found again on the next resize).
    sqlx::query("DELETE FROM provisioned_servers WHERE workspace_id = $1 AND provider = 'aws'")
        .bind(workspace)
        .execute(&mut *tx)
        .await?;
    Entry::new("integration.aws.disconnect", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("integration", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(status_of(&state, workspace).await?))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub locations: Vec<ProviderLocation>,
    pub server_types: Vec<ServerTypeOffer>,
}

pub(crate) async fn connected(state: &AppState, workspace: Id) -> AppResult<Ec2Provider> {
    provider(state, workspace)
        .await?
        .ok_or_else(|| AppError::Conflict("connect AWS in workspace settings first".into()))
}

/// Locations and orderable server types with monthly prices (compute, root disk and Elastic IP).
pub async fn catalog(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Catalog>> {
    let aws = connected(&state, grant.id()).await?;
    let (locations, server_types) = futures::try_join!(aws.locations(), aws.server_types())?;
    Ok(Json(Catalog {
        locations,
        server_types,
    }))
}

pub async fn ssh_keys(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<ProviderSshKey>>> {
    let aws = connected(&state, grant.id()).await?;
    Ok(Json(aws.ssh_keys().await?))
}
