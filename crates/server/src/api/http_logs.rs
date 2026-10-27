//! HTTP requests served by the edge (see `crate::http_logs`):
//! `GET /api/environments/{id}/http-logs?since&until&service&deployment&status&method&path&limit`,
//! and request rate / errors / latency for charts:
//! `GET /api/service-instances/{id}/http-metrics?range=` and
//! `GET /api/projects/{id}/http-metrics?environment=&range=`.

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};

use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http_logs::{self, Found, Point};
use crate::metrics::Range;
use crate::state::AppState;

const METHODS: &[&str] = &[
    "GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "CONNECT", "TRACE",
];

#[derive(Deserialize)]
pub struct Params {
    since: i64,
    until: Option<i64>,
    service: Option<Id>,
    deployment: Option<Id>,
    /// `2xx` … `5xx`, or one code (`404`).
    status: Option<String>,
    method: Option<String>,
    /// Substring of the path.
    path: Option<String>,
    limit: Option<i64>,
}

pub async fn search(
    State(state): State<AppState>,
    grant: Grant,
    Query(p): Query<Params>,
) -> AppResult<Json<Found>> {
    let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
    let until = p.until.unwrap_or(now).min(now + 1000);
    let oldest = now - i64::from(http_logs::RETENTION_DAYS) * 24 * 3_600_000;
    let since = p.since.max(oldest);
    if since >= until {
        return Err(AppError::bad_request("since must be before until"));
    }
    let (status_class, status) = match p.status.as_deref() {
        None | Some("") => (None, None),
        Some("2xx") => (Some(2), None),
        Some("3xx") => (Some(3), None),
        Some("4xx") => (Some(4), None),
        Some("5xx") => (Some(5), None),
        Some(code) => match code.parse::<i32>() {
            Ok(c) if code.len() == 3 && (100..=599).contains(&c) => (None, Some(c)),
            _ => {
                return Err(AppError::bad_request(
                    "status is 2xx, 3xx, 4xx, 5xx or a status code",
                ));
            }
        },
    };
    let method = p.method.filter(|m| !m.is_empty());
    if method.as_deref().is_some_and(|m| !METHODS.contains(&m)) {
        return Err(AppError::bad_request("unknown method"));
    }
    if p.path.as_deref().is_some_and(|q| q.len() > 500) {
        return Err(AppError::bad_request("the path filter is too long"));
    }
    let q = http_logs::Query {
        since_ms: since,
        until_ms: until,
        service: p.service,
        deployment: p.deployment,
        status_class,
        status,
        method,
        path: p.path,
        limit: p.limit.unwrap_or(1000),
    };
    Ok(Json(http_logs::search(&state.db, grant.id(), &q).await?))
}

#[derive(Deserialize)]
pub struct MetricsParams {
    #[serde(default)]
    range: Range,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpMetrics {
    range: Range,
    step_secs: i64,
    points: Vec<Point>,
}

pub async fn instance_metrics(
    State(state): State<AppState>,
    grant: Grant,
    Query(p): Query<MetricsParams>,
) -> AppResult<Json<HttpMetrics>> {
    let points = http_logs::series(&state.db, &[grant.id()], p.range).await?;
    Ok(Json(HttpMetrics {
        range: p.range,
        step_secs: p.range.span_and_step().1,
        points,
    }))
}

#[derive(Deserialize)]
pub struct ProjectParams {
    environment: Id,
    #[serde(default)]
    range: Range,
}

/// Every service of the project in one environment, summed.
pub async fn project_metrics(
    State(state): State<AppState>,
    grant: Grant,
    Query(p): Query<ProjectParams>,
) -> AppResult<Json<HttpMetrics>> {
    let instances: Vec<Id> = sqlx::query_scalar(
        "SELECT i.id FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         WHERE s.project_id = $1 AND e.project_id = $1 AND i.environment_id = $2",
    )
    .bind(grant.id())
    .bind(p.environment)
    .fetch_all(&state.db)
    .await?;
    let points = http_logs::series(&state.db, &instances, p.range).await?;
    Ok(Json(HttpMetrics {
        range: p.range,
        step_secs: p.range.span_and_step().1,
        points,
    }))
}
