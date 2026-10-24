//! `GET /api/service-instances/{id}/metrics?range=1h|6h|24h|7d`: the
//! service's CPU, memory and network usage (see `crate::metrics`).

use axum::Json;
use axum::extract::{Query, State};
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
