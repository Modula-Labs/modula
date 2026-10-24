//! A project's outgoing webhooks (see `crate::webhooks`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;
use crate::webhooks::{self, Delivery, Event};

/// Per project; more is a mistake, not a use case.
const MAX_PER_PROJECT: i64 = 20;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Webhook {
    id: Id,
    url: String,
    events: Vec<String>,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    last_status: Option<i32>,
    last_error: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
pub struct Created {
    #[serde(flatten)]
    webhook: Webhook,
    /// Shown once: receivers verify `X-Modula-Signature` with it.
    secret: String,
}

#[derive(Deserialize)]
pub struct Create {
    url: String,
    events: Vec<Event>,
}

macro_rules! columns {
    () => {
        "id, url, events, created_at, last_status, last_error, last_at"
    };
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Webhook>>> {
    let rows = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM webhooks WHERE project_id = $1 ORDER BY created_at"
    ))
    .bind(grant.id())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Create>,
) -> AppResult<Json<Created>> {
    let project = grant.id();
    let workspace = grant.workspace_id()?;
    let url = webhooks::validate_url(&req.url)?;
    let mut events: Vec<&str> = req.events.iter().map(|e| e.as_str()).collect();
    events.sort_unstable();
    events.dedup();
    if events.is_empty() {
        return Err(AppError::bad_request("pick at least one event"));
    }
    let id = Id::new();
    let secret = format!(
        "whsec_{}",
        hex::encode(modula_crypto::random_bytes::<24>().map_err(anyhow::Error::from)?)
    );
    let row = id.to_string();
    let sealed = state
        .keys
        .seal(&state.db, secret.as_bytes(), webhooks::aad(workspace, &row))
        .await?;
    let mut tx = state.db.begin().await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM webhooks WHERE project_id = $1")
        .bind(project)
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_PER_PROJECT {
        return Err(AppError::Conflict(format!(
            "a project can have at most {MAX_PER_PROJECT} webhooks"
        )));
    }
    let webhook: Webhook = sqlx::query_as(concat!(
        "INSERT INTO webhooks (id, project_id, url, events, secret_sealed, created_by)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING ",
        columns!()
    ))
    .bind(id)
    .bind(project)
    .bind(url.as_str())
    .bind(&events)
    .bind(&sealed)
    .bind(actor.user_id)
    .fetch_one(&mut *tx)
    .await?;
    Entry::new("webhook.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("webhook", id)
        // The host only: a path or query may carry the receiver's token.
        .metadata(json!({ "project": project, "host": url.host_str(), "events": events }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(Created { webhook, secret }))
}

pub async fn delete(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<StatusCode> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM webhooks WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("webhook.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("webhook", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Sends a `test` event right away and returns what the endpoint answered.
/// Test deliveries per user: each one is an outbound request we make.
const TEST_PER_USER: Limit = Limit::per_minute(10);

pub async fn test(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
) -> AppResult<Json<Delivery>> {
    state
        .limiter
        .check("webhook-test", &actor.user_id.to_string(), TEST_PER_USER)
        .await?;
    let id = grant.id();
    let body = json!({
        "type": "test",
        "timestamp": OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(anyhow::Error::from)?,
        "webhook": { "id": id },
    });
    Ok(Json(webhooks::deliver(&state, id, "test", &body).await?))
}
