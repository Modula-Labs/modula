//! Deployments: immutable snapshots of a service instance's config and
//! variables, placed on a node and driven by the node's events.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
use crate::deploy::{self, LiveFilter, env, placement};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    pub id: Uuid,
    pub service_instance_id: Uuid,
    pub node_id: Option<Uuid>,
    pub status: String,
    pub image: String,
    pub trigger: String,
    pub error: Option<String>,
    pub created_by: Option<Uuid>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentEvent {
    pub phase: String,
    pub message: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentDetail {
    #[serde(flatten)]
    pub deployment: Deployment,
    pub events: Vec<DeploymentEvent>,
}

macro_rules! select_deployments {
    ($rest:literal) => {
        concat!(
            "SELECT id, service_instance_id, node_id, status, image, trigger, error, created_by,
                    created_at, updated_at, finished_at FROM deployments ",
            $rest
        )
    };
}

async fn load(state: &AppState, id: Uuid) -> AppResult<DeploymentDetail> {
    let deployment: Deployment = sqlx::query_as(select_deployments!("WHERE id = $1"))
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound)?;
    let events = sqlx::query_as(
        "SELECT phase, message, at FROM deployment_events WHERE deployment_id = $1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(&state.db)
    .await?;
    Ok(DeploymentDetail { deployment, events })
}

pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<DeploymentDetail>> {
    let instance = grant.id();
    let (env_id, config): (Uuid, serde_json::Value) =
        sqlx::query_as("SELECT environment_id, config FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    let cfg: ServiceConfig = serde_json::from_value(config.clone()).map_err(anyhow::Error::from)?;
    cfg.validate()?;
    let id = Uuid::now_v7();
    let vars = env::resolve(&state.keys, &state.db, env_id, instance).await?;
    let env_sealed = env::seal_snapshot(&state.keys, &state.db, env_id, id, &vars).await?;

    let mut tx = state.db.begin().await?;
    sqlx::query("SELECT id FROM service_instances WHERE id = $1 FOR UPDATE")
        .bind(instance)
        .execute(&mut *tx)
        .await?;
    let node = placement::choose(&mut tx, instance).await?;
    // A newer deployment supersedes any still in progress (the running one
    // keeps serving until the new one is healthy).
    let superseded: Vec<Option<Uuid>> = sqlx::query_scalar(
        "UPDATE deployments SET status = 'cancelled', finished_at = now(), updated_at = now()
         WHERE service_instance_id = $1 AND status IN ('queued', 'deploying') RETURNING node_id",
    )
    .bind(instance)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO deployments (id, service_instance_id, node_id, status, image, config, env_sealed, trigger, created_by)
         VALUES ($1, $2, $3, 'deploying', $4, $5, $6, 'manual', $7)",
    )
    .bind(id)
    .bind(instance)
    .bind(node)
    .bind(cfg.image())
    .bind(&config)
    .bind(&env_sealed)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'queued', $2)",
    )
    .bind(id)
    .bind(format!("Deploying {}", cfg.image()))
    .execute(&mut *tx)
    .await?;
    let mut kick: Vec<Uuid> = superseded.into_iter().flatten().collect();
    kick.push(node);
    kick.sort();
    kick.dedup();
    for n in &kick {
        deploy::bump(&mut *tx, *n).await?;
    }
    Entry::new("deployment.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("deployment", id)
        .metadata(json!({ "serviceInstance": instance, "image": cfg.image(), "node": node }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&kick);
    load(&state, id).await.map(Json)
}

pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Deployment>>> {
    Ok(Json(
        sqlx::query_as(select_deployments!(
            "WHERE service_instance_id = $1 ORDER BY created_at DESC LIMIT 50"
        ))
        .bind(grant.id())
        .fetch_all(&state.db)
        .await?,
    ))
}

pub async fn get(State(state): State<AppState>, grant: Grant) -> AppResult<Json<DeploymentDetail>> {
    load(&state, grant.id()).await.map(Json)
}

pub async fn remove(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<DeploymentDetail>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let removed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM deployments WHERE id = $1 AND status IN ('queued', 'deploying', 'active', 'crashed')",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if removed == 0 {
        return Err(AppError::Conflict("this deployment is not running".into()));
    }
    let nodes = deploy::remove_live(&mut tx, LiveFilter::Deployment, id).await?;
    Entry::new("deployment.remove", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("deployment", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    load(&state, id).await.map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogQuery {
    pub tail: Option<u32>,
    pub follow: Option<bool>,
}

/// Closes the node-side log stream when the browser goes away.
struct LogGuard {
    state: AppState,
    request_id: String,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        // No runtime means the process is shutting down; nothing to cancel.
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (state, id) = (self.state.clone(), std::mem::take(&mut self.request_id));
        rt.spawn(async move { state.hub.close_logs(&id).await });
    }
}

/// Live deploy logs as Server-Sent Events, fetched from the node over its
/// stream. Secret values are masked before anything leaves the server.
pub async fn logs(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<LogQuery>,
) -> AppResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let id = grant.id();
    let (node, env_id, env_sealed): (Option<Uuid>, Uuid, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT d.node_id, i.environment_id, d.env_sealed
         FROM deployments d JOIN service_instances i ON i.id = d.service_instance_id WHERE d.id = $1",
    )
    .bind(id)
    .fetch_one(&state.db)
    .await?;
    let node = node.ok_or_else(|| AppError::Conflict("this deployment has no server".into()))?;
    let vars = match &env_sealed {
        Some(s) => env::open_snapshot(&state.keys, &state.db, env_id, id, s).await?,
        None => env::EnvMap::new(),
    };
    let redactor = env::Redactor::new(&vars);
    drop(vars);
    let tail = q.tail.unwrap_or(200).min(5000);
    let follow = q.follow.unwrap_or(true);
    let (request_id, rx) = state
        .hub
        .open_logs(node, id, tail, follow)
        .await
        .ok_or_else(|| {
            AppError::Conflict("the server running this deployment is offline".into())
        })?;
    let guard = LogGuard {
        state: state.clone(),
        request_id,
    };

    let stream = futures::stream::unfold(
        (rx, redactor, guard, VecDeque::<Event>::new(), false),
        |(mut rx, redactor, guard, mut queue, mut finished)| async move {
            loop {
                if let Some(ev) = queue.pop_front() {
                    return Some((Ok(ev), (rx, redactor, guard, queue, finished)));
                }
                if finished {
                    return None;
                }
                match rx.recv().await {
                    Some(chunk) => {
                        for l in chunk.lines {
                            let data = json!({
                                "ts": l.ts_unix_ms,
                                "stderr": l.stderr,
                                "text": redactor.redact(&l.text),
                            });
                            queue.push_back(Event::default().event("log").data(data.to_string()));
                        }
                        if !chunk.error.is_empty() {
                            let data = json!({ "message": chunk.error.chars().take(500).collect::<String>() });
                            queue.push_back(Event::default().event("error").data(data.to_string()));
                        }
                        if chunk.done {
                            queue.push_back(Event::default().event("end").data("{}"));
                            finished = true;
                        }
                    }
                    None => {
                        queue.push_back(Event::default().event("end").data("{}"));
                        finished = true;
                    }
                }
            }
        },
    );
    let stream = stream.take_until(state.shutdown.clone().cancelled_owned());
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
