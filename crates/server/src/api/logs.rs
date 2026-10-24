//! `GET /api/environments/{id}/logs?since&until&q&service&limit`: stored
//! output of every service in the environment (see `crate::logs`).

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::Deserialize;

use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::logs::{self, Found};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct Params {
    since: i64,
    until: Option<i64>,
    q: Option<String>,
    service: Option<Id>,
    limit: Option<i64>,
}

pub async fn search(
    State(state): State<AppState>,
    grant: Grant,
    Query(p): Query<Params>,
) -> AppResult<Json<Found>> {
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let until = p.until.unwrap_or(now).min(now + 1000);
    let oldest = now - i64::from(logs::RETENTION_DAYS) * 24 * 3_600_000;
    let since = p.since.max(oldest);
    if since >= until {
        return Err(AppError::bad_request("since must be before until"));
    }
    if p.q.as_deref().is_some_and(|q| q.len() > 500) {
        return Err(AppError::bad_request("the search text is too long"));
    }
    let q = logs::Query {
        since_ms: since,
        until_ms: until,
        text: p.q,
        service: p.service,
        limit: p.limit.unwrap_or(1000),
    };
    Ok(Json(logs::search(&state.db, grant.id(), &q).await?))
}
