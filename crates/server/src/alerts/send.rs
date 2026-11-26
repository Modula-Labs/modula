//! Delivering an alert to a channel through the webhooks' SSRF-guarded
//! client (or, for email, the workspace's email provider), and the record
//! of every send (the dashboard's history).

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
    deliver_with(state, channel, alert, None).await
}

/// [`deliver`], with the email an email channel gets instead of the one
/// formatted from `alert` (the weekly report has its own).
pub async fn deliver_with(
    state: &AppState,
    channel: &Channel,
    alert: &Alert,
    mail: Option<&format::Mail>,
) -> AppResult<Delivery> {
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
    let out = if channel.kind == "email" {
        let mail = mail.cloned().unwrap_or_else(|| format::email_in(alert, Some(channel.workspace_id)));
        send_email(state, channel.workspace_id, &url, mail).await?
    } else {
        let (body, headers) = request(&channel.kind, alert, secret.as_deref().map(|s| &s[..]))?;
        webhooks::post(&url, &headers, body).await?
    };
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

/// Emails `alert` to `to` through the workspace's Cloudflare account. A
/// missing connection or sender is a failed delivery, not an error, so it
/// shows on the channel and in the history.
async fn send_email(
    state: &AppState,
    workspace: Id,
    to: &str,
    mail: format::Mail,
) -> AppResult<Delivery> {
    use crate::integrations::cloudflare::{self, email};
    let failed = |status: Option<u16>, msg: &str| Delivery {
        status,
        error: Some(msg.to_owned()),
    };
    let Some(conn) = cloudflare::connection(state, workspace).await? else {
        return Ok(failed(None, "connect Cloudflare to send email"));
    };
    let (Some(account), Some(from)) = (&conn.metadata.account_id, &conn.metadata.email_from) else {
        return Ok(failed(
            None,
            "set the sender address in Settings → Integrations → Cloudflare → Email",
        ));
    };
    let provider = email::Cloudflare {
        client: &conn.client,
        account_id: account,
    };
    let msg = email::Email {
        from: from.clone(),
        from_name: "Modula".into(),
        to: to.to_owned(),
        subject: mail.subject,
        text: mail.text,
        html: mail.html,
    };
    Ok(match email::EmailProvider::send(&provider, &msg).await {
        Ok(_) => Delivery {
            status: Some(200),
            error: None,
        },
        Err(e) => failed(e.status, &e.message),
    })
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

/// The channels a workspace's weekly traffic report goes to.
pub(crate) async fn report_channels(state: &AppState, workspace: Id) -> sqlx::Result<Vec<Channel>> {
    sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM notification_channels
         WHERE id IN (SELECT channel_id FROM traffic_report_channels WHERE workspace_id = $1)
         ORDER BY created_at"
    ))
    .bind(workspace)
    .fetch_all(&state.db)
    .await
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
