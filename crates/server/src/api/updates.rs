//! Updates (instance admins): versions, the update mode and maintenance
//! window, the last upgrade and each server's version, "Update now". See
//! `crate::updates`; the dashboard's side is web/src/pages/settings/Updates.tsx.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::updates::{self, Mode, release, rollout, window};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub current_version: &'static str,
    pub latest_version: Option<String>,
    pub update_available: bool,
    /// The newer release's page.
    pub release_notes_url: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub checked_at: Option<OffsetDateTime>,
    pub check_error: Option<String>,
    pub mode: Mode,
    pub channel: String,
    pub window: WindowBody,
    /// When the next maintenance window opens (RFC 3339).
    pub next_window: Option<String>,
    /// Whether this server can upgrade itself (install.sh set it up).
    pub can_upgrade: bool,
    pub last_run: Option<Run>,
    pub servers: Vec<ServerVersion>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WindowBody {
    /// Weekdays, 0 = Sunday … 6 = Saturday.
    pub days: Vec<u8>,
    /// Local start time, `HH:MM` (24-hour).
    pub time: String,
    /// IANA time zone. Stored empty (the default) means the server's own,
    /// which is what is shown.
    pub timezone: String,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: Id,
    pub from_version: String,
    pub to_version: String,
    pub trigger: String,
    /// `requested` (waiting for modula-upgrade), `running` (it took the
    /// request), `succeeded`, `failed` or `rolled_back`.
    pub status: String,
    pub log: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub requested_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerVersion {
    pub node_id: Id,
    pub name: String,
    pub version: Option<String>,
    /// `current`, `behind` (waiting its turn), `upgrading`, `failed`,
    /// `manual` (installed without the upgrade units, so it is upgraded by hand) or `offline`.
    pub status: &'static str,
    pub error: Option<String>,
}

/// Weekday bits (bit 0 = Monday, as stored) ↔ day numbers (0 = Sunday).
fn days_from_mask(mask: i16) -> Vec<u8> {
    (0..7u8)
        .filter(|d| mask & (1 << ((d + 6) % 7)) != 0)
        .collect()
}

fn mask_from_days(days: &[u8]) -> i16 {
    days.iter().fold(0, |m, d| m | (1 << ((d + 6) % 7)))
}

fn window_body(s: &updates::Settings) -> WindowBody {
    WindowBody {
        days: days_from_mask(s.window_days),
        time: format!("{:02}:{:02}", s.window_minute / 60, s.window_minute % 60),
        timezone: if s.timezone.is_empty() {
            window::system_timezone_name()
        } else {
            s.timezone.clone()
        },
    }
}

/// A server's place in the rollout to this server's version. `last` is its
/// latest upgrade (status, target version, started); failures from before
/// `retried_at` no longer count.
fn server_status(
    connected: bool,
    version: Option<&str>,
    can_upgrade: bool,
    last: Option<(&str, &str, OffsetDateTime)>,
    retried_at: Option<OffsetDateTime>,
) -> &'static str {
    let behind = version.is_none_or(|v| release::is_newer(updates::VERSION, v));
    match last {
        Some(("running", _, _)) => return "upgrading",
        Some(("failed", to, started))
            if behind && to == updates::VERSION && retried_at.is_none_or(|r| started > r) =>
        {
            return "failed";
        }
        _ => {}
    }
    if !connected {
        "offline"
    } else if !behind {
        "current"
    } else if !can_upgrade {
        "manual"
    } else {
        "behind"
    }
}

type ServerRow = (
    Id,
    String,
    Option<String>,
    bool,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<OffsetDateTime>,
);

async fn load(state: &AppState) -> AppResult<UpdateStatus> {
    let s = updates::settings(&state.db).await?;
    let mut last_run: Option<Run> = sqlx::query_as(
        "SELECT id, from_version, to_version, trigger, status, log, created_at AS requested_at, finished_at
         FROM instance_upgrades ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(&state.db)
    .await?;
    let upgrade_dir = state
        .cfg
        .releases
        .upgrade_dir
        .as_ref()
        .filter(|d| d.is_dir());
    if let (Some(run), Some(dir)) = (last_run.as_mut(), upgrade_dir)
        && run.status == "requested"
        && !dir.join("request").exists()
    {
        // modula-upgrade took the request and is running the installer.
        run.status = "running".into();
    }

    let rows: Vec<ServerRow> = sqlx::query_as(
        "SELECT n.id, n.name, n.node_version, coalesce(n.info->'capabilities' ? 'upgrade', false),
                u.status, u.to_version, u.error, u.started_at
         FROM nodes n
         LEFT JOIN LATERAL (SELECT status, to_version, error, started_at FROM node_upgrades
                            WHERE node_id = n.id ORDER BY started_at DESC LIMIT 1) u ON true
         WHERE n.status <> 'revoked' ORDER BY n.name, n.id",
    )
    .fetch_all(&state.db)
    .await?;
    let servers = rows
        .into_iter()
        .map(
            |(id, name, version, can_upgrade, status, to, error, started)| {
                let last = match (status.as_deref(), to.as_deref(), started) {
                    (Some(s), Some(t), Some(at)) => Some((s, t, at)),
                    _ => None,
                };
                let status = server_status(
                    state.hub.is_connected(id),
                    version.as_deref(),
                    can_upgrade,
                    last,
                    s.rollout_cleared_at,
                );
                ServerVersion {
                    node_id: id,
                    name,
                    version,
                    status,
                    error: error.filter(|_| status == "failed"),
                }
            },
        )
        .collect();

    let available = s.available().map(str::to_owned);
    Ok(UpdateStatus {
        current_version: updates::VERSION,
        update_available: available.is_some(),
        release_notes_url: available
            .as_deref()
            .map(|v| release::release_page(&state.cfg.releases.url, v)),
        latest_version: s.latest_version.clone(),
        checked_at: s.checked_at,
        check_error: s.check_error.clone(),
        mode: s.mode(),
        channel: s.channel.clone(),
        window: window_body(&s),
        next_window: s
            .window()
            .next_start(jiff::Timestamp::now())
            .map(|t| t.to_string()),
        can_upgrade: upgrade_dir.is_some(),
        last_run,
        servers,
    })
}

pub async fn get(State(state): State<AppState>) -> AppResult<Json<UpdateStatus>> {
    load(&state).await.map(Json)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateSettings {
    pub mode: Mode,
    #[serde(default = "stable")]
    pub channel: String,
    pub window: WindowBody,
}

fn stable() -> String {
    "stable".into()
}

/// `HH:MM` → minutes after midnight.
fn parse_time(s: &str) -> Option<i16> {
    let (h, m) = s.split_once(':')?;
    let (h, m): (i16, i16) = (h.parse().ok()?, m.parse().ok()?);
    ((0..24).contains(&h) && (0..60).contains(&m) && s.len() == 5).then_some(h * 60 + m)
}

pub async fn put(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<UpdateSettings>,
) -> AppResult<Json<UpdateStatus>> {
    if req.channel != "stable" {
        return Err(AppError::bad_request("the only channel is stable"));
    }
    let minute = parse_time(&req.window.time)
        .ok_or_else(|| AppError::bad_request("time must be HH:MM (24-hour)"))?;
    if req.window.days.is_empty() || req.window.days.iter().any(|d| *d > 6) {
        return Err(AppError::bad_request(
            "choose at least one day (0 = Sunday … 6 = Saturday)",
        ));
    }
    let tz = req.window.timezone.trim();
    if !window::valid_timezone(tz) {
        return Err(AppError::bad_request("unknown time zone"));
    }
    // The server's own zone, as shown, stays "the server's own" (and follows it).
    let tz = if tz == window::system_timezone_name() {
        ""
    } else {
        tz
    };
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "UPDATE instance_updates SET mode = $1, window_days = $2, window_minute = $3, timezone = $4,
                updated_by = $5, updated_at = now()",
    )
    .bind(req.mode.as_str())
    .bind(mask_from_days(&req.window.days))
    .bind(minute)
    .bind(tz)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    Entry::new("instance.updates.update", &meta)
        .actor(&actor)
        .metadata(json!({
            "mode": req.mode.as_str(),
            "days": req.window.days,
            "time": req.window.time,
            "timezone": tz,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    load(&state).await.map(Json)
}

/// Check for a new release now.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct CheckParams {
    /// The dashboard's automatic check when it opens: skipped when the
    /// release server was asked within the last minute (many tabs, many
    /// admins). "Check now" always asks.
    auto: bool,
}

pub async fn check(
    State(state): State<AppState>,
    Query(p): Query<CheckParams>,
) -> AppResult<Json<UpdateStatus>> {
    let recent: bool = sqlx::query_scalar(
        "SELECT coalesce(checked_at > now() - interval '1 minute', false) FROM instance_updates",
    )
    .fetch_one(&state.db)
    .await?;
    if !(p.auto && recent) {
        updates::check(&state).await?;
    }
    load(&state).await.map(Json)
}

/// "Update now": upgrade the control plane to the newest release (the other
/// servers follow), or, when it is current, resume a paused server rollout.
pub async fn run(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
) -> AppResult<Json<UpdateStatus>> {
    let s = updates::settings(&state.db).await?;
    if let Some(latest) = s.available() {
        updates::request(&state, latest, updates::Trigger::Manual(&actor, &meta)).await?;
        return load(&state).await.map(Json);
    }
    let current = load(&state).await?;
    if !current.servers.iter().any(|n| n.status == "failed") {
        return Err(AppError::Conflict("everything is up to date".into()));
    }
    rollout::retry(&state).await?;
    Entry::new("instance.upgrade.servers_retry", &meta)
        .actor(&actor)
        .metadata(json!({ "to": updates::VERSION }))
        .record(&state.db)
        .await?;
    load(&state).await.map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(parse_time("03:00"), Some(180));
        assert_eq!(parse_time("23:59"), Some(1439));
        for bad in ["3:00", "24:00", "12:60", "noon", "", "03:00:00"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
    }

    #[test]
    fn days_round_trip_between_sunday_first_and_the_stored_mask() {
        // Stored bit 6 = Sunday (the default) ↔ day 0.
        assert_eq!(days_from_mask(64), vec![0]);
        assert_eq!(mask_from_days(&[0]), 64);
        // Monday = bit 0 = day 1; Saturday = bit 5 = day 6.
        assert_eq!(mask_from_days(&[1, 6]), 1 | 32);
        for days in [vec![0, 1, 2, 3, 4, 5, 6], vec![3], vec![0, 6]] {
            assert_eq!(days_from_mask(mask_from_days(&days)), days);
        }
    }

    #[test]
    fn server_statuses() {
        let old = Some("0.0.1");
        let v = updates::VERSION;
        let t = OffsetDateTime::now_utc();
        assert_eq!(server_status(true, Some(v), true, None, None), "current");
        assert_eq!(server_status(false, Some(v), true, None, None), "offline");
        assert_eq!(server_status(true, old, true, None, None), "behind");
        assert_eq!(server_status(true, old, false, None, None), "manual");
        assert_eq!(
            server_status(true, old, true, Some(("running", v, t)), None),
            "upgrading"
        );
        assert_eq!(
            server_status(true, old, true, Some(("failed", v, t)), None),
            "failed"
        );
        // Retried since: waiting its turn again.
        let later = Some(t + time::Duration::seconds(1));
        assert_eq!(
            server_status(true, old, true, Some(("failed", v, t)), later),
            "behind"
        );
        // A failure for another version doesn't count.
        assert_eq!(
            server_status(true, old, true, Some(("failed", "0.0.2", t)), None),
            "behind"
        );
    }
}
