//! Delivering an alert to a channel through the webhooks' SSRF-guarded
//! client, and the record of every send (the dashboard's history).

use modula_id::Id;

use super::{Alert, format};
use crate::error::AppResult;
use crate::state::AppState;
use crate::webhooks::{self, Delivery};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Channel {
    pub id: Id,
    pub workspace_id: Id,
    pub kind: String,
    pub name: String,
    pub url_sealed: Vec<u8>,
    pub secret_sealed: Option<Vec<u8>>,
}

macro_rules! columns {
    () => {
        "id, workspace_id, kind, name, url_sealed, secret_sealed"
    };
}

/// Extra request headers.
pub type Headers = Vec<(&'static str, String)>;

/// The body and headers `channel` gets for `alert` (generic webhooks are
/// signed with the channel's secret like project webhooks).
pub fn request(
    kind: &str,
    alert: &Alert,
    secret: Option<&[u8]>,
) -> anyhow::Result<(Vec<u8>, Headers)> {
    let body = match kind {
        "slack" => format::slack(alert),
        "discord" => format::discord(alert),
        _ => format::webhook(alert),
    };
    let body = serde_json::to_vec(&body)?;
    let mut headers = Vec::new();
    if kind == "webhook" {
        headers.push(("x-modula-event", format::event_name(alert.phase).to_owned()));
        headers.push(("x-modula-delivery", Id::new().to_string()));
        if let Some(s) = secret {
            headers.push(("x-modula-signature", webhooks::sign(s, &body)));
        }
    }
    Ok((body, headers))
}

/// Sends `alert` to `channel`, records the outcome on the channel and in the
/// history, and returns it.
pub async fn deliver(state: &AppState, channel: &Channel, alert: &Alert) -> AppResult<Delivery> {
    let row = channel.id.to_string();
    let url = state
        .keys
        .open(
            &state.db,
            &channel.url_sealed,
            super::url_aad(channel.workspace_id, &row),
        )
        .await?;
    let url = String::from_utf8_lossy(&url).into_owned();
    let secret = match &channel.secret_sealed {
        Some(s) => Some(
            state
                .keys
                .open(&state.db, s, super::secret_aad(channel.workspace_id, &row))
                .await?,
        ),
        None => None,
    };
    let (body, headers) = request(&channel.kind, alert, secret.as_deref().map(|s| &s[..]))?;
    let out = webhooks::post(&url, &headers, body).await?;
    sqlx::query(
        "UPDATE notification_channels SET last_status = $2, last_error = $3, last_at = now()
         WHERE id = $1",
    )
    .bind(channel.id)
    .bind(out.status.map(i32::from))
    .bind(&out.error)
    .execute(&state.db)
    .await?;
    sqlx::query(
        "INSERT INTO alert_sends (workspace_id, rule_id, rule_name, channel_id, channel_name,
                                  channel_kind, phase, subject, title, status, error)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(channel.workspace_id)
    .bind(alert.rule_id)
    .bind(&alert.rule_name)
    .bind(channel.id)
    .bind(&channel.name)
    .bind(&channel.kind)
    .bind(alert.phase.as_str())
    .bind(&alert.subject)
    .bind(&alert.title)
    .bind(out.status.map(i32::from))
    .bind(&out.error)
    .execute(&state.db)
    .await?;
    Ok(out)
}

pub async fn channel(state: &AppState, id: Id) -> AppResult<Option<Channel>> {
    Ok(sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM notification_channels WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?)
}

/// Sends `alert` to every channel of `rule`. A failing channel is recorded
/// and the others still get it; nothing is retried.
pub async fn notify(state: &AppState, rule: Id, alert: &Alert) -> AppResult<()> {
    let channels: Vec<Channel> = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM notification_channels
         WHERE id IN (SELECT channel_id FROM alert_rule_channels WHERE rule_id = $1)
         ORDER BY created_at"
    ))
    .bind(rule)
    .fetch_all(&state.db)
    .await?;
    for c in &channels {
        match deliver(state, c, alert).await {
            Ok(d) if d.error.is_some() => {
                tracing::warn!(channel = %c.id, error = ?d.error, "alert delivery failed")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(channel = %c.id, error = ?e, "alert delivery failed"),
        }
    }
    Ok(())
}
