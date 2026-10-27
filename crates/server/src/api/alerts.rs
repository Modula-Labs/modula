//! A workspace's alert channels, rules and notification history (see
//! `crate::alerts`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::alerts::{self, Alert, Kind, Phase, send};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;
use crate::webhooks::{self, Delivery};

const MAX_CHANNELS: i64 = 20;
const MAX_RULES: i64 = 100;
/// Test sends per user: each one is an outbound request we make.
const TEST_PER_USER: Limit = Limit::per_minute(10);

// ------------------------------------------------------------------ channels

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Channel {
    id: Id,
    kind: String,
    name: String,
    host: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    last_status: Option<i32>,
    last_error: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    last_at: Option<OffsetDateTime>,
}

macro_rules! channel_columns {
    () => {
        "id, kind, name, host, created_at, last_status, last_error, last_at"
    };
}

#[derive(Serialize)]
pub struct CreatedChannel {
    #[serde(flatten)]
    channel: Channel,
    /// Webhook channels only, shown once: receivers verify `X-Modula-Signature` with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateChannel {
    kind: String,
    name: String,
    url: String,
}

fn name(raw: &str) -> AppResult<String> {
    let n = raw.trim();
    if n.is_empty() || n.chars().count() > 100 {
        return Err(AppError::bad_request("the name must be 1–100 characters"));
    }
    Ok(n.to_owned())
}

pub async fn list_channels(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<Channel>>> {
    let rows = sqlx::query_as(concat!(
        "SELECT ",
        channel_columns!(),
        " FROM notification_channels WHERE workspace_id = $1 ORDER BY created_at"
    ))
    .bind(grant.id())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

pub async fn create_channel(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateChannel>,
) -> AppResult<Json<CreatedChannel>> {
    let workspace = grant.id();
    if !matches!(req.kind.as_str(), "slack" | "discord" | "webhook") {
        return Err(AppError::bad_request(
            "the kind must be slack, discord or webhook",
        ));
    }
    let name = name(&req.name)?;
    let url = webhooks::validate_url(&req.url)?;
    let host = url.host_str().unwrap_or_default().to_owned();
    let id = Id::new();
    let row = id.to_string();
    let url_sealed = state
        .keys
        .seal(
            &state.db,
            url.as_str().as_bytes(),
            alerts::url_aad(workspace, &row),
        )
        .await?;
    let (secret, secret_sealed) = if req.kind == "webhook" {
        let secret = format!(
            "whsec_{}",
            hex::encode(modula_crypto::random_bytes::<24>().map_err(anyhow::Error::from)?)
        );
        let sealed = state
            .keys
            .seal(
                &state.db,
                secret.as_bytes(),
                alerts::secret_aad(workspace, &row),
            )
            .await?;
        (Some(secret), Some(sealed))
    } else {
        (None, None)
    };
    let mut tx = state.db.begin().await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notification_channels WHERE workspace_id = $1")
            .bind(workspace)
            .fetch_one(&mut *tx)
            .await?;
    if count >= MAX_CHANNELS {
        return Err(AppError::Conflict(format!(
            "a workspace can have at most {MAX_CHANNELS} notification channels"
        )));
    }
    let channel: Channel = sqlx::query_as(concat!(
        "INSERT INTO notification_channels (id, workspace_id, kind, name, host, url_sealed, secret_sealed, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING ",
        channel_columns!()
    ))
    .bind(id)
    .bind(workspace)
    .bind(&req.kind)
    .bind(&name)
    .bind(&host)
    .bind(&url_sealed)
    .bind(&secret_sealed)
    .bind(actor.user_id)
    .fetch_one(&mut *tx)
    .await?;
    Entry::new("notification_channel.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("notification_channel", id)
        // The host only: the path carries the receiver's token.
        .metadata(json!({ "kind": req.kind, "host": host }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(CreatedChannel { channel, secret }))
}

pub async fn delete_channel(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<StatusCode> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM notification_channels WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("notification_channel.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("notification_channel", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Sends a test notification right away and returns what the endpoint answered.
pub async fn test_channel(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
) -> AppResult<Json<Delivery>> {
    state
        .limiter
        .check("alert-test", &actor.user_id.to_string(), TEST_PER_USER)
        .await?;
    let channel = send::channel(&state, grant.id())
        .await?
        .ok_or(AppError::NotFound)?;
    let alert = Alert {
        phase: Phase::Test,
        rule_id: None,
        rule_name: "Test".into(),
        kind: None,
        subject: format!("channel:{}", channel.id),
        title: "Test notification from Modula".into(),
        reason: format!("Alerts sent to {} arrive like this one.", channel.name),
        project: None,
        environment: None,
        service: None,
        server: None,
        link: format!("{}/settings", state.public_origin()),
        at: OffsetDateTime::now_utc(),
    };
    Ok(Json(send::deliver(&state, &channel, &alert).await?))
}

// ------------------------------------------------------------------ rules

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    id: Id,
    name: String,
    kind: String,
    project_id: Option<Id>,
    project_name: Option<String>,
    service_id: Option<Id>,
    service_name: Option<String>,
    threshold: Option<f32>,
    window_minutes: Option<i32>,
    cooldown_minutes: i32,
    enabled: bool,
    channel_ids: Vec<String>,
    /// Subjects whose condition holds now.
    firing: i64,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

macro_rules! rule_select {
    () => {
        "SELECT r.id, r.name, r.kind, r.project_id, p.name AS project_name, r.service_id,
                s.name AS service_name, r.threshold, r.window_minutes, r.cooldown_minutes,
                r.enabled, r.created_at,
                coalesce((SELECT array_agg(c.channel_id::text ORDER BY c.channel_id)
                          FROM alert_rule_channels c WHERE c.rule_id = r.id), '{}') AS channel_ids,
                (SELECT count(*) FROM alert_states a WHERE a.rule_id = r.id AND a.firing) AS firing
         FROM alert_rules r
         LEFT JOIN projects p ON p.id = r.project_id
         LEFT JOIN services s ON s.id = r.service_id "
    };
}

async fn rule(db: &sqlx::PgPool, id: Id) -> AppResult<Rule> {
    Ok(sqlx::query_as(concat!(rule_select!(), "WHERE r.id = $1"))
        .bind(id)
        .fetch_one(db)
        .await?)
}

pub async fn list_rules(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Rule>>> {
    let rows = sqlx::query_as(concat!(
        rule_select!(),
        "WHERE r.workspace_id = $1 ORDER BY r.created_at"
    ))
    .bind(grant.id())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// Workspace-wide rules and the ones scoped to the project.
pub async fn project_rules(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<Rule>>> {
    let workspace = grant.workspace_id()?;
    let rows = sqlx::query_as(concat!(
        rule_select!(),
        "WHERE r.workspace_id = $1 AND (r.project_id IS NULL OR r.project_id = $2)
         ORDER BY r.created_at"
    ))
    .bind(workspace)
    .bind(grant.id())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

/// A rule's checked settings: (threshold, window, cooldown).
pub fn settings(
    kind: Kind,
    threshold: Option<f32>,
    window: Option<i32>,
    cooldown: Option<i32>,
) -> AppResult<(Option<f32>, Option<i32>, i32)> {
    let bad = AppError::bad_request;
    let threshold = if kind.needs_threshold() {
        match threshold {
            Some(t) if t.is_finite() && t > 0.0 && t <= 100.0 => Some(t),
            _ => return Err(bad("the threshold must be a percentage between 1 and 100")),
        }
    } else {
        None
    };
    let window = match kind.window_range() {
        Some((lo, hi)) => match window {
            Some(w) if (lo..=hi).contains(&w) => Some(w),
            _ => {
                return Err(AppError::bad_request(format!(
                    "the duration must be {lo}–{hi} minutes"
                )));
            }
        },
        None => None,
    };
    let cooldown = cooldown.unwrap_or(30);
    if !(0..=1440).contains(&cooldown) {
        return Err(bad("the cooldown must be 0–1440 minutes"));
    }
    Ok((threshold, window, cooldown))
}

/// The channel ids, deduplicated, all of `workspace`'s.
async fn channels_of(
    db: &mut sqlx::PgConnection,
    workspace: Id,
    ids: &[Id],
) -> AppResult<Vec<String>> {
    let mut ids: Vec<String> = ids.iter().map(ToString::to_string).collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(AppError::bad_request("pick at least one channel"));
    }
    let found: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_channels WHERE workspace_id = $1 AND id = ANY($2::text[])",
    )
    .bind(workspace)
    .bind(&ids)
    .fetch_one(&mut *db)
    .await?;
    if found != ids.len() as i64 {
        return Err(AppError::bad_request("unknown channel"));
    }
    Ok(ids)
}

async fn set_channels(db: &mut sqlx::PgConnection, rule: Id, ids: &[String]) -> AppResult<()> {
    sqlx::query("DELETE FROM alert_rule_channels WHERE rule_id = $1")
        .bind(rule)
        .execute(&mut *db)
        .await?;
    sqlx::query(
        "INSERT INTO alert_rule_channels (rule_id, channel_id) SELECT $1, unnest($2::text[])",
    )
    .bind(rule)
    .bind(ids)
    .execute(&mut *db)
    .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRule {
    name: String,
    kind: String,
    #[serde(default)]
    project_id: Option<Id>,
    #[serde(default)]
    service_id: Option<Id>,
    #[serde(default)]
    threshold: Option<f32>,
    #[serde(default)]
    window_minutes: Option<i32>,
    #[serde(default)]
    cooldown_minutes: Option<i32>,
    channel_ids: Vec<Id>,
}

pub async fn create_rule(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateRule>,
) -> AppResult<Json<Rule>> {
    let workspace = grant.id();
    let name = name(&req.name)?;
    let kind = Kind::parse(&req.kind).ok_or_else(|| AppError::bad_request("unknown alert kind"))?;
    let (threshold, window, cooldown) = settings(
        kind,
        req.threshold,
        req.window_minutes,
        req.cooldown_minutes,
    )?;
    let mut tx = state.db.begin().await?;
    if let Some(p) = req.project_id {
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM projects WHERE id = $1 AND workspace_id = $2)",
        )
        .bind(p)
        .bind(workspace)
        .fetch_one(&mut *tx)
        .await?;
        if !ok {
            return Err(AppError::bad_request("unknown project"));
        }
    }
    if let Some(s) = req.service_id {
        let Some(p) = req.project_id else {
            return Err(AppError::bad_request("a service scope needs its project"));
        };
        let ok: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM services WHERE id = $1 AND project_id = $2)",
        )
        .bind(s)
        .bind(p)
        .fetch_one(&mut *tx)
        .await?;
        if !ok {
            return Err(AppError::bad_request("unknown service"));
        }
    }
    let channels = channels_of(&mut tx, workspace, &req.channel_ids).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM alert_rules WHERE workspace_id = $1")
        .bind(workspace)
        .fetch_one(&mut *tx)
        .await?;
    if count >= MAX_RULES {
        return Err(AppError::Conflict(format!(
            "a workspace can have at most {MAX_RULES} alert rules"
        )));
    }
    let id = Id::new();
    sqlx::query(
        "INSERT INTO alert_rules (id, workspace_id, name, kind, project_id, service_id, threshold,
                                  window_minutes, cooldown_minutes, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
    )
    .bind(id)
    .bind(workspace)
    .bind(&name)
    .bind(kind.as_str())
    .bind(req.project_id)
    .bind(req.service_id)
    .bind(threshold)
    .bind(window)
    .bind(cooldown)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    set_channels(&mut tx, id, &channels).await?;
    Entry::new("alert_rule.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("alert_rule", id)
        .metadata(
            json!({ "kind": kind.as_str(), "project": req.project_id, "service": req.service_id }),
        )
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(rule(&state.db, id).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRule {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    threshold: Option<f32>,
    #[serde(default)]
    window_minutes: Option<i32>,
    #[serde(default)]
    cooldown_minutes: Option<i32>,
    #[serde(default)]
    channel_ids: Option<Vec<Id>>,
}

pub async fn update_rule(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateRule>,
) -> AppResult<Json<Rule>> {
    let id = grant.id();
    let workspace = grant.workspace_id()?;
    let current = rule(&state.db, id).await?;
    let kind = Kind::parse(&current.kind).ok_or(AppError::NotFound)?;
    let name = match &req.name {
        Some(n) => name(n)?,
        None => current.name,
    };
    let (threshold, window, cooldown) = settings(
        kind,
        req.threshold.or(current.threshold),
        req.window_minutes.or(current.window_minutes),
        Some(req.cooldown_minutes.unwrap_or(current.cooldown_minutes)),
    )?;
    let enabled = req.enabled.unwrap_or(current.enabled);
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE alert_rules SET name = $2, enabled = $3, threshold = $4, window_minutes = $5,
                cooldown_minutes = $6 WHERE id = $1",
    )
    .bind(id)
    .bind(&name)
    .bind(enabled)
    .bind(threshold)
    .bind(window)
    .bind(cooldown)
    .execute(&mut *tx)
    .await?;
    if let Some(ids) = &req.channel_ids {
        let channels = channels_of(&mut tx, workspace, ids).await?;
        set_channels(&mut tx, id, &channels).await?;
    }
    // Disabled: its open alerts are forgotten (no "resolved" for them).
    if !enabled {
        sqlx::query("DELETE FROM alert_states WHERE rule_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    Entry::new("alert_rule.update", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("alert_rule", id)
        .metadata(json!({ "enabled": enabled }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(rule(&state.db, id).await?))
}

pub async fn delete_rule(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<StatusCode> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    sqlx::query("DELETE FROM alert_rules WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    Entry::new("alert_rule.delete", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("alert_rule", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ------------------------------------------------------------------ history

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Send {
    id: i64,
    #[serde(with = "time::serde::rfc3339")]
    at: OffsetDateTime,
    rule_id: Option<Id>,
    rule_name: String,
    channel_id: Option<Id>,
    channel_name: String,
    channel_kind: String,
    phase: String,
    subject: String,
    title: String,
    status: Option<i32>,
    error: Option<String>,
}

/// The last 100 notifications sent.
pub async fn history(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Send>>> {
    let rows = sqlx::query_as(
        "SELECT id, at, rule_id, rule_name, channel_id, channel_name, channel_kind, phase,
                subject, title, status, error
         FROM alert_sends WHERE workspace_id = $1 ORDER BY id DESC LIMIT 100",
    )
    .bind(grant.id())
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_settings_by_kind() {
        // Events: thresholds and windows are dropped.
        assert_eq!(
            settings(Kind::DeploymentFailed, Some(50.0), Some(5), None).unwrap(),
            (None, None, 30)
        );
        assert_eq!(
            settings(Kind::Cpu, Some(80.0), Some(5), Some(0)).unwrap(),
            (Some(80.0), Some(5), 0)
        );
        assert!(settings(Kind::Cpu, None, Some(5), None).is_err());
        assert!(settings(Kind::Cpu, Some(80.0), None, None).is_err());
        assert!(settings(Kind::Cpu, Some(80.0), Some(61), None).is_err());
        assert!(settings(Kind::Memory, Some(101.0), Some(5), None).is_err());
        assert!(settings(Kind::Memory, Some(f32::NAN), Some(5), None).is_err());
        assert_eq!(
            settings(Kind::Disk, Some(90.0), Some(5), None).unwrap(),
            (Some(90.0), None, 30)
        );
        assert_eq!(
            settings(Kind::NodeOffline, None, Some(1440), None).unwrap(),
            (None, Some(1440), 30)
        );
        assert!(settings(Kind::NodeOffline, None, Some(0), None).is_err());
        assert!(settings(Kind::BackupFailed, None, None, Some(1441)).is_err());
    }
}
