//! Deployments: immutable snapshots of a service instance's config and
//! variables, placed on a node and driven by the node's events.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::{Stream, StreamExt};
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
use crate::deploy::start::{NewDeployment, Trigger, start};
use crate::deploy::{self, LiveFilter, env};
use crate::error::{AppError, AppResult};
use crate::github::Commit;
use crate::http::meta::RequestMeta;
use crate::state::AppState;

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Deployment {
    pub id: Id,
    pub service_instance_id: Id,
    pub node_id: Option<Id>,
    pub status: String,
    pub image: String,
    pub image_digest: Option<String>,
    pub trigger: String,
    pub commit_sha: Option<String>,
    pub commit_message: Option<String>,
    pub commit_author: Option<String>,
    pub commit_author_avatar: Option<String>,
    /// `owner/repo@branch` or the image, from the config snapshot.
    pub source: String,
    /// `github` or `image` (`upload` once uploads exist).
    pub source_kind: String,
    /// The deployment a redeploy or rollback repeated.
    pub source_deployment_id: Option<Id>,
    pub error: Option<String>,
    pub created_by: Option<Id>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub finished_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub build_started_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub build_finished_at: Option<OffsetDateTime>,
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
            "SELECT id, service_instance_id, node_id, status, image, image_digest, trigger,
                    commit_sha, commit_message, commit_author,
                    'https://github.com/' || commit_author_login || '.png?size=64' AS commit_author_avatar,
                    CASE WHEN config->'source'->>'type' = 'repo'
                         THEN (config->'source'->>'repo') || '@' || (config->'source'->>'branch')
                         ELSE config->'source'->>'image' END AS source,
                    CASE WHEN config->'source'->>'type' = 'repo' THEN 'github' ELSE 'image' END AS source_kind,
                    source_deployment_id,
                    error, created_by, created_at, updated_at, finished_at,
                    build_started_at, build_finished_at
             FROM deployments ",
            $rest
        )
    };
}

async fn load(state: &AppState, id: Id) -> AppResult<DeploymentDetail> {
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
    let (env_id, config): (Id, serde_json::Value) =
        sqlx::query_as("SELECT environment_id, config FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    let cfg: ServiceConfig = serde_json::from_value(config).map_err(anyhow::Error::from)?;
    // Resolve the branch head before taking any locks: it is a network call.
    let commit = match cfg.repo() {
        Some((repo, branch, _)) => Some(
            crate::github::app::branch_head(&state, grant.workspace_id()?, repo, branch).await?,
        ),
        None => None,
    };
    let source = cfg.source_label();
    let mut tx = state.db.begin().await?;
    let started = start(
        &state,
        &mut tx,
        env_id,
        NewDeployment {
            instance,
            config: cfg,
            trigger: Trigger::Manual,
            created_by: Some(actor.user_id),
            commit: commit.clone(),
            prebuilt: None,
            source_deployment: None,
        },
    )
    .await?;
    Entry::new("deployment.create", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("deployment", started.id)
        .metadata(json!({
            "serviceInstance": instance,
            "source": source,
            "commit": commit.map(|c| c.sha),
            "node": started.node,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&started.kick);
    load(&state, started.id).await.map(Json)
}

/// Deploy the same source again: the same image, or the same commit rebuilt
/// (source code is never stored, so a commit is always built afresh). Config
/// comes from the deployment's snapshot; variables are the current ones.
pub async fn redeploy(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<DeploymentDetail>> {
    rerun(state, actor, grant, meta, Trigger::Redeploy).await
}

/// Rebuild and redeploy an earlier deployment's commit (repository sources only).
pub async fn rollback(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<DeploymentDetail>> {
    rerun(state, actor, grant, meta, Trigger::Rollback).await
}

async fn rerun(
    state: AppState,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    trigger: Trigger,
) -> AppResult<Json<DeploymentDetail>> {
    let source_id = grant.id();
    #[allow(clippy::type_complexity)]
    let (instance, env_id, config, sha, message, author, login): (
        Id,
        Id,
        serde_json::Value,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT d.service_instance_id, i.environment_id, d.config, d.commit_sha, d.commit_message,
                d.commit_author, d.commit_author_login
         FROM deployments d JOIN service_instances i ON i.id = d.service_instance_id WHERE d.id = $1",
    )
    .bind(source_id)
    .fetch_one(&state.db)
    .await?;
    let cfg: ServiceConfig = serde_json::from_value(config).map_err(anyhow::Error::from)?;
    let commit = sha.map(|sha| Commit {
        sha,
        message: message.unwrap_or_default(),
        author: author.unwrap_or_default(),
        author_login: login,
    });
    if cfg.repo().is_some() != commit.is_some() {
        return Err(AppError::Conflict(
            "this deployment's source can't be deployed again".into(),
        ));
    }
    if trigger == Trigger::Rollback && commit.is_none() {
        return Err(AppError::Conflict(
            "only deployments built from a Git commit can be rolled back to".into(),
        ));
    }
    let mut tx = state.db.begin().await?;
    let started = start(
        &state,
        &mut tx,
        env_id,
        NewDeployment {
            instance,
            config: cfg,
            trigger,
            created_by: Some(actor.user_id),
            commit: commit.clone(),
            prebuilt: None,
            source_deployment: Some(source_id),
        },
    )
    .await?;
    Entry::new(
        match trigger {
            Trigger::Rollback => "deployment.rollback",
            _ => "deployment.redeploy",
        },
        &meta,
    )
    .actor(&actor)
    .workspace(grant.workspace_id()?)
    .target("deployment", started.id)
    .metadata(json!({ "from": source_id, "commit": commit.map(|c| c.sha), "node": started.node }))
    .record(&mut *tx)
    .await?;
    tx.commit().await?;
    state.hub.kick_all(&started.kick);
    load(&state, started.id).await.map(Json)
}

/// Restart the running container in place (same deployment, same image).
pub async fn restart(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<DeploymentDetail>> {
    let id = grant.id();
    let mut tx = state.db.begin().await?;
    let node: Option<Option<Id>> = sqlx::query_scalar(
        "UPDATE deployments SET restart_generation = restart_generation + 1, updated_at = now()
         WHERE id = $1 AND status IN ('active', 'crashed') RETURNING node_id",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Some(node)) = node else {
        return Err(AppError::Conflict(
            "only a running deployment can be restarted".into(),
        ));
    };
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'restart', 'Restart requested')",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    deploy::bump(&mut *tx, node).await?;
    Entry::new("deployment.restart", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("deployment", id)
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&[node]);
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
        "SELECT count(*) FROM deployments WHERE id = $1 AND status IN ('queued', 'building', 'deploying', 'active', 'crashed')",
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
    /// `runtime` (default) or `build`.
    pub kind: Option<String>,
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
    let (node, env_id, env_sealed): (Option<Id>, Id, Option<Vec<u8>>) = sqlx::query_as(
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
    let kind = match q.kind.as_deref() {
        None | Some("runtime") => modula_proto::node::v1::LogKind::LogRuntime,
        Some("build") => modula_proto::node::v1::LogKind::LogBuild,
        Some(_) => return Err(AppError::bad_request("kind must be runtime or build")),
    };
    let (request_id, rx) = state
        .hub
        .open_logs(node, id, tail, follow, kind)
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
