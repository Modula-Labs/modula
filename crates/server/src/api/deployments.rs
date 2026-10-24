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
use crate::deploy::config::{ServiceConfig, Source};
use crate::deploy::start::{LocalGit, NewDeployment, Trigger, start};
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
    /// What the person deploying wrote when applying pending changes.
    pub message: Option<String>,
    pub commit_author: Option<String>,
    pub commit_author_avatar: Option<String>,
    /// `owner/repo@branch`, the image, or `Uploaded via CLI`, from the config snapshot.
    pub source: String,
    /// `github`, `image` or `upload`.
    pub source_kind: String,
    /// Uploads: the local branch and whether uncommitted changes were included
    /// (as the CLI reported them), and whether the files are still kept (so
    /// the deployment can be redeployed or rolled back to).
    pub source_branch: Option<String>,
    pub source_dirty: Option<bool>,
    pub upload_available: Option<bool>,
    /// The deployment a redeploy or rollback repeated.
    pub source_deployment_id: Option<Id>,
    pub error: Option<String>,
    /// Where the deployment is hosted when not on the servers (`cloudflare`).
    pub hosting: Option<String>,
    /// Where such a deployment is served, once published.
    pub url: Option<String>,
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
    /// Serverless: the last wake from `sleeping`, and how long it took.
    #[serde(with = "time::serde::rfc3339::option")]
    pub woke_at: Option<OffsetDateTime>,
    pub cold_start_ms: Option<i32>,
    /// Active, but a replica of it crashed and isn't running again yet.
    pub degraded: bool,
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
            "SELECT d.id, d.service_instance_id, d.node_id, d.status, d.image, d.image_digest, d.trigger,
                    d.commit_sha, d.commit_message, d.message, d.commit_author,
                    'https://avatars.githubusercontent.com/' || d.commit_author_login || '?s=64' AS commit_author_avatar,
                    CASE d.config->'source'->>'type'
                         WHEN 'repo' THEN (d.config->'source'->>'repo') || '@' || (d.config->'source'->>'branch')
                         WHEN 'upload' THEN 'Uploaded via CLI'
                         WHEN 'function' THEN coalesce((d.config->'source'->>'repo') || '@' || (d.config->'source'->>'branch'), 'Function code')
                         ELSE d.config->'source'->>'image' END AS source,
                    CASE WHEN d.config->'source' ? 'repo' THEN 'github'
                         WHEN d.config->'source'->>'type' = 'upload' THEN 'upload'
                         WHEN d.config->'source'->>'type' = 'function' THEN 'function'
                         ELSE 'image' END AS source_kind,
                    d.source_branch, d.source_dirty,
                    CASE WHEN d.config->'source'->>'type' = 'upload'
                         THEN u.id IS NOT NULL AND u.purged_at IS NULL END AS upload_available,
                    d.source_deployment_id,
                    d.error, d.config->'hosting'->>'type' AS hosting, d.url,
                    d.created_by, d.created_at, d.updated_at, d.finished_at,
                    d.build_started_at, d.build_finished_at, d.woke_at, d.cold_start_ms, ",
            $crate::degraded_sql!(),
            " AS degraded
             FROM deployments d LEFT JOIN source_uploads u ON u.id = d.upload_id ",
            $rest
        )
    };
}

async fn load(state: &AppState, id: Id) -> AppResult<DeploymentDetail> {
    let deployment: Deployment = sqlx::query_as(select_deployments!("WHERE d.id = $1"))
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

/// Optional body of `POST /api/service-instances/{id}/deployments`. Without
/// a `source`, the instance's configured source is deployed.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateDeployment {
    pub source: Option<DeploySource>,
    /// Uploads: a description (`modula up -m`).
    pub message: Option<String>,
    /// Uploads: the local Git state the files were packed from (display only).
    pub git: Option<LocalGitInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeploySource {
    /// A `source_uploads` id of this instance (`POST …/uploads`).
    pub upload: Id,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalGitInfo {
    pub commit: Option<String>,
    pub branch: Option<String>,
    /// The commit's subject, used when there is no message.
    pub subject: Option<String>,
    pub dirty: Option<bool>,
}

pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    body: Option<Json<CreateDeployment>>,
) -> AppResult<Json<DeploymentDetail>> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let workspace = grant.workspace_id()?;
    let id = match req.source {
        Some(DeploySource { upload }) => {
            let git = local_git(req.message, req.git.unwrap_or_default())?;
            deploy_upload(&state, &actor, workspace, &meta, grant.id(), upload, git).await?
        }
        None if req.message.is_some() || req.git.is_some() => {
            return Err(AppError::bad_request(
                "message and git only apply to uploaded sources",
            ));
        }
        None => deploy_instance(&state, &actor, workspace, &meta, grant.id(), None).await?,
    };
    load(&state, id).await.map(Json)
}

/// Checks what the CLI reported about the local checkout.
fn local_git(message: Option<String>, git: LocalGitInfo) -> AppResult<LocalGit> {
    if let Some(c) = &git.commit
        && !(c.len() == 40
            && c.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    {
        return Err(AppError::bad_request(
            "git.commit must be a full lowercase hex sha",
        ));
    }
    if let Some(b) = &git.branch {
        crate::deploy::config::validate_branch(b)?;
    }
    let clean = |s: String| -> Option<String> {
        let s: String = s.trim().chars().filter(|c| *c != '\0').take(1000).collect();
        (!s.is_empty()).then_some(s)
    };
    Ok(LocalGit {
        commit: git.commit,
        message: message.and_then(clean).or(git.subject.and_then(clean)),
        branch: git.branch,
        dirty: git.dirty,
    })
}

/// Build and deploy an upload: the instance's current config (builder,
/// commands, root directory…) with the upload as its source. The instance's
/// own source setting is not changed.
pub async fn deploy_upload(
    state: &AppState,
    actor: &Actor,
    workspace: Id,
    meta: &RequestMeta,
    instance: Id,
    upload: Id,
    git: LocalGit,
) -> AppResult<Id> {
    let found: Option<(Id, bool, String)> = sqlx::query_as(
        "SELECT service_instance_id, purged_at IS NULL, sha256 FROM source_uploads WHERE id = $1",
    )
    .bind(upload)
    .fetch_optional(&state.db)
    .await?;
    let sha256 = match found {
        Some((owner, true, sha)) if owner == instance => sha,
        Some((owner, false, _)) if owner == instance => {
            return Err(AppError::Conflict(
                "this upload is no longer kept; run `modula up` again".into(),
            ));
        }
        _ => return Err(AppError::bad_request("unknown upload for this service")),
    };
    let (env_id, config): (Id, serde_json::Value) =
        sqlx::query_as("SELECT environment_id, config FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    let mut cfg: ServiceConfig = serde_json::from_value(config).map_err(anyhow::Error::from)?;
    cfg.source = Source::Upload {
        upload,
        root_dir: cfg.root_dir().map(str::to_owned),
    };
    let mut tx = state.db.begin().await?;
    let started = start(
        state,
        &mut tx,
        env_id,
        NewDeployment {
            instance,
            config: cfg,
            trigger: Trigger::Upload,
            created_by: Some(actor.user_id),
            commit: None,
            local_git: Some(git.clone()),
            prebuilt: None,
            source_deployment: None,
        },
    )
    .await?;
    Entry::new("deployment.create", meta)
        .actor(actor)
        .workspace(workspace)
        .target("deployment", started.id)
        .metadata(json!({
            "serviceInstance": instance,
            "source": "upload",
            "upload": upload,
            "sha256": sha256,
            "commit": git.commit,
            "dirty": git.dirty,
            "node": started.node,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    state.hub.kick_all(&started.kick);
    Ok(started.id)
}

/// Deploy an instance's current config and variables (a manual deploy).
pub async fn deploy_instance(
    state: &AppState,
    actor: &Actor,
    workspace: Id,
    meta: &RequestMeta,
    instance: Id,
    message: Option<&str>,
) -> AppResult<Id> {
    let (env_id, config): (Id, serde_json::Value) =
        sqlx::query_as("SELECT environment_id, config FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(&state.db)
            .await?;
    let mut cfg: ServiceConfig = serde_json::from_value(config).map_err(anyhow::Error::from)?;
    // A local-files service deploys its latest kept upload (new variables or
    // settings, same files), like Railway's redeploy of a `railway up`.
    let mut local_git = None;
    if matches!(cfg.source, Source::Local { .. }) {
        #[derive(sqlx::FromRow)]
        struct Latest {
            id: Id,
            commit_sha: Option<String>,
            commit_message: Option<String>,
            source_branch: Option<String>,
            source_dirty: Option<bool>,
        }
        let latest: Option<Latest> = sqlx::query_as(
            "SELECT u.id, d.commit_sha, d.commit_message, d.source_branch, d.source_dirty
                 FROM source_uploads u
                 LEFT JOIN LATERAL (
                     SELECT commit_sha, commit_message, source_branch, source_dirty
                     FROM deployments WHERE upload_id = u.id ORDER BY created_at DESC LIMIT 1
                 ) d ON true
                 WHERE u.service_instance_id = $1 AND u.purged_at IS NULL
                 ORDER BY u.created_at DESC LIMIT 1",
        )
        .bind(instance)
        .fetch_optional(&state.db)
        .await?;
        let Some(latest) = latest else {
            return Err(AppError::Conflict(
                "this service deploys local files and has none uploaded yet; run `modula up` in its directory".into(),
            ));
        };
        cfg.source = Source::Upload {
            upload: latest.id,
            root_dir: cfg.root_dir().map(str::to_owned),
        };
        local_git = Some(LocalGit {
            commit: latest.commit_sha,
            message: latest.commit_message,
            branch: latest.source_branch,
            dirty: latest.source_dirty,
        });
    }
    // Resolve the branch head before taking any locks: it is a network call.
    let commit = match cfg.repo() {
        Some((repo, branch, _)) => {
            Some(crate::github::app::branch_head(state, workspace, repo, branch).await?)
        }
        None => None,
    };
    let source = cfg.source_label();
    let mut tx = state.db.begin().await?;
    let started = start(
        state,
        &mut tx,
        env_id,
        NewDeployment {
            instance,
            config: cfg,
            trigger: Trigger::Manual,
            created_by: Some(actor.user_id),
            commit: commit.clone(),
            local_git,
            prebuilt: None,
            source_deployment: None,
        },
    )
    .await?;
    if let Some(m) = message {
        sqlx::query("UPDATE deployments SET message = $2 WHERE id = $1")
            .bind(started.id)
            .bind(m)
            .execute(&mut *tx)
            .await?;
    }
    Entry::new("deployment.create", meta)
        .actor(actor)
        .workspace(workspace)
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
    Ok(started.id)
}

/// Deploy the same source again: the same image, the same commit rebuilt
/// (repository code is never stored, so a commit is always built afresh), or
/// the same upload rebuilt while it is kept (see `deploy::upload::purge`).
/// Config comes from the deployment's snapshot; variables are the current ones.
pub async fn redeploy(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<DeploymentDetail>> {
    rerun(state, actor, grant, meta, Trigger::Redeploy).await
}

/// Rebuild and redeploy an earlier deployment's commit or kept upload.
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
    let r: RerunRow = sqlx::query_as(
        "SELECT d.service_instance_id, i.environment_id, d.config, d.commit_sha, d.commit_message,
                d.commit_author, d.commit_author_login, d.source_branch, d.source_dirty,
                u.id IS NOT NULL AND u.purged_at IS NULL AS upload_kept
         FROM deployments d JOIN service_instances i ON i.id = d.service_instance_id
         LEFT JOIN source_uploads u ON u.id = d.upload_id
         WHERE d.id = $1",
    )
    .bind(source_id)
    .fetch_one(&state.db)
    .await?;
    let (instance, env_id) = (r.service_instance_id, r.environment_id);
    let cfg: ServiceConfig = serde_json::from_value(r.config).map_err(anyhow::Error::from)?;
    // Uploads keep their local Git details for display; they never drive a build.
    let (commit, local_git) = if cfg.upload().is_some() {
        if !r.upload_kept {
            return Err(AppError::Conflict(
                "the files uploaded for this deployment are no longer kept; run `modula up` again"
                    .into(),
            ));
        }
        let git = LocalGit {
            commit: r.commit_sha,
            message: r.commit_message,
            branch: r.source_branch,
            dirty: r.source_dirty,
        };
        (None, Some(git))
    } else {
        let commit = r.commit_sha.map(|sha| Commit {
            sha,
            message: r.commit_message.unwrap_or_default(),
            author: r.commit_author.unwrap_or_default(),
            author_login: r.commit_author_login,
        });
        (commit, None)
    };
    if cfg.repo().is_some() != commit.is_some() {
        return Err(AppError::Conflict(
            "this deployment's source can't be deployed again".into(),
        ));
    }
    if trigger == Trigger::Rollback && commit.is_none() && local_git.is_none() {
        return Err(AppError::Conflict(
            "only deployments built from a Git commit or an upload can be rolled back to".into(),
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
            local_git,
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

#[derive(sqlx::FromRow)]
struct RerunRow {
    service_instance_id: Id,
    environment_id: Id,
    config: serde_json::Value,
    commit_sha: Option<String>,
    commit_message: Option<String>,
    commit_author: Option<String>,
    commit_author_login: Option<String>,
    source_branch: Option<String>,
    source_dirty: Option<bool>,
    upload_kept: bool,
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
         WHERE id = $1 AND status IN ('active', 'sleeping', 'crashed') AND config->'hosting' IS NULL
           AND config->'cronSchedule' IS NULL
         RETURNING node_id",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Some(node)) = node else {
        return Err(AppError::Conflict(
            "only a deployment running on your servers can be restarted".into(),
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
            "WHERE d.service_instance_id = $1 ORDER BY d.created_at DESC LIMIT 50"
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
        "SELECT count(*) FROM deployments WHERE id = $1 AND status IN ('waiting_ci', 'queued', 'building', 'deploying', 'active', 'sleeping', 'crashed')",
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
    // On shutdown (an upgrade or restart) the stream ends with a `restart`
    // event, so the viewer reconnects in a moment to whichever process is up.
    let restarting = state.shutdown.clone();
    let restart_hint = futures::stream::once(async move { restarting.is_cancelled() }).filter_map(
        |cancelled| async move {
            cancelled.then(|| {
                Ok(Event::default()
                    .event("restart")
                    .retry(Duration::from_secs(1))
                    .data("{}"))
            })
        },
    );
    let stream = stream
        .take_until(state.shutdown.clone().cancelled_owned())
        .chain(restart_hint);
    Ok(Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15))))
}
