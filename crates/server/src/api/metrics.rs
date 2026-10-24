//! `GET /api/service-instances/{id}/metrics?range=1h|6h|24h|7d`: the
//! service's CPU, memory and network usage (see `crate::metrics`).

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use serde::{Deserialize, Serialize};

use crate::authz::Grant;
use crate::error::AppResult;
use crate::metrics::{self, Point, Range};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct Params {
    #[serde(default)]
    range: Range,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    range: Range,
    step_secs: i64,
    /// The server running the latest deployment (0 when unknown).
    cpus: i64,
    memory_total_bytes: i64,
    /// The service's memory limit, if it has one.
    memory_limit_bytes: Option<i64>,
    points: Vec<Point>,
}

pub async fn get(
    State(state): State<AppState>,
    grant: Grant,
    Query(p): Query<Params>,
) -> AppResult<Json<Metrics>> {
    let instance = grant.id();
    let points = metrics::series(&state.db, instance, p.range).await?;
    let (cpus, memory_total_bytes, memory_mb): (Option<i64>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "SELECT (n.info->>'cpus')::bigint, (n.info->>'memoryBytes')::bigint,
                    (i.config->>'memoryMb')::bigint
             FROM service_instances i
             LEFT JOIN LATERAL (
                 SELECT node_id FROM deployments WHERE service_instance_id = i.id
                 ORDER BY created_at DESC LIMIT 1
             ) d ON true
             LEFT JOIN nodes n ON n.id = COALESCE(d.node_id, i.node_id)
             WHERE i.id = $1",
        )
        .bind(instance)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(Metrics {
        range: p.range,
        step_secs: p.range.span_and_step().1,
        cpus: cpus.unwrap_or(0),
        memory_total_bytes: memory_total_bytes.unwrap_or(0),
        memory_limit_bytes: memory_mb.filter(|m| *m > 0).map(|m| m * 1024 * 1024),
        points,
    }))
}

#[derive(Deserialize)]
pub struct ProjectParams {
    environment: Id,
    #[serde(default)]
    range: Range,
}

/// `GET /api/projects/{id}/metrics?environment=&range=`: every service of
/// the project in one environment, summed; the totals are those of the
/// servers the services run on.
pub async fn project(
    State(state): State<AppState>,
    grant: Grant,
    Query(p): Query<ProjectParams>,
) -> AppResult<Json<Metrics>> {
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
    let points = metrics::series_of(&state.db, &instances, p.range).await?;
    let (cpus, memory_total_bytes, memory_limit): (Option<i64>, Option<i64>, Option<i64>) =
        sqlx::query_as(
            "WITH placed AS (
                 SELECT DISTINCT COALESCE(d.node_id, i.node_id) AS node_id
                 FROM service_instances i
                 LEFT JOIN LATERAL (
                     SELECT node_id FROM deployments WHERE service_instance_id = i.id
                     ORDER BY created_at DESC LIMIT 1
                 ) d ON true
                 WHERE i.id = ANY($1)
             )
             SELECT sum((n.info->>'cpus')::bigint)::bigint, sum((n.info->>'memoryBytes')::bigint)::bigint,
                    (SELECT CASE WHEN bool_and((config->>'memoryMb')::bigint > 0)
                                 THEN sum((config->>'memoryMb')::bigint) END::bigint
                     FROM service_instances WHERE id = ANY($1))
             FROM placed JOIN nodes n ON n.id = placed.node_id",
        )
        .bind(&instances)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(Metrics {
        range: p.range,
        step_secs: p.range.span_and_step().1,
        cpus: cpus.unwrap_or(0),
        memory_total_bytes: memory_total_bytes.unwrap_or(0),
        memory_limit_bytes: memory_limit.filter(|m| *m > 0).map(|m| m * 1024 * 1024),
        points,
    }))
}
