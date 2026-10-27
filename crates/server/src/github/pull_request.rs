//! Pull request previews. In a project with previews on, every open pull
//! request of a repository its base environment deploys from gets an
//! ephemeral environment `pr-<number>`: a duplicate of the base without its
//! sealed variables or bucket credentials (the pull request's code mustn't
//! see production's secrets), whose services of that repository deploy the
//! pull request's head branch. Only pull requests from the repository itself
//! (no forks) by someone with write access get one. A single comment on the
//! pull request (found again by a hidden marker) lists each service's URL and
//! status; closing the pull request deletes the environment.

use std::collections::BTreeMap;

use modula_id::Id;
use reqwest::Method;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::{Value, json};

use super::Commit;
use super::app::{self, TokenScope};
use super::push::{InstallationRef, deploy, is_sha, should_deploy};
use crate::api::projects::{self, NewEnvironment, PrRef};
use crate::deploy::config::{ServiceConfig, validate_branch, validate_repo};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::jobs::{self, Job, NewJob};
use crate::state::AppState;

/// Open previews per project; further pull requests get none.
pub const MAX_PREVIEWS: i64 = 10;
pub const COMMENT_JOB: &str = "github.pr_comment";
/// Pages of changed files read (GitHub lists at most 3000 files).
const MAX_FILE_PAGES: u32 = 30;

#[derive(Deserialize)]
pub struct PullRequestEvent {
    action: String,
    number: i64,
    pull_request: PullRequest,
    repository: Repository,
    installation: Option<InstallationRef>,
}

#[derive(Deserialize)]
struct PullRequest {
    title: String,
    #[serde(default)]
    author_association: String,
    user: User,
    head: Head,
}

#[derive(Deserialize)]
struct User {
    login: String,
}

#[derive(Deserialize)]
struct Head {
    #[serde(rename = "ref")]
    git_ref: String,
    sha: String,
    /// None when the fork it came from was deleted.
    repo: Option<Repository>,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

impl PullRequestEvent {
    /// Why this pull request gets no preview, if it doesn't.
    fn untrusted(&self) -> Option<&'static str> {
        let same_repo = self
            .pull_request
            .head
            .repo
            .as_ref()
            .is_some_and(|r| r.full_name.eq_ignore_ascii_case(&self.repository.full_name));
        if !same_repo {
            return Some("pull requests from forks get no preview");
        }
        // Owners, organization members and collaborators can push to the
        // repository anyway; anyone else's code doesn't run here.
        if !matches!(
            self.pull_request.author_association.as_str(),
            "OWNER" | "MEMBER" | "COLLABORATOR"
        ) {
            return Some("the author has no write access to the repository");
        }
        None
    }
}

/// `pull_request`: create, update or delete previews. `app` is the App
/// whose secret signed the delivery.
pub async fn handle_pull_request(
    state: &AppState,
    meta: &RequestMeta,
    app: i64,
    delivery: &str,
    ev: &PullRequestEvent,
) -> AppResult<Value> {
    let Some(installation) = &ev.installation else {
        return Ok(json!({}));
    };
    let repo = ev.repository.full_name.as_str();
    let Ok(number) = i32::try_from(ev.number) else {
        return Ok(json!({}));
    };
    if validate_repo(repo).is_err() || number < 1 {
        return Ok(json!({}));
    }
    match ev.action.as_str() {
        "opened" | "reopened" | "synchronize" => {
            if let Some(why) = ev.untrusted() {
                tracing::info!(repo, number, delivery, why, "pull request ignored");
                return Ok(json!({ "ignored": why }));
            }
            open(state, meta, app, installation.id, delivery, ev, number).await
        }
        "closed" => close(state, meta, app, installation.id, repo, number).await,
        _ => Ok(json!({})),
    }
}

async fn open(
    state: &AppState,
    meta: &RequestMeta,
    app: i64,
    installation: i64,
    delivery: &str,
    ev: &PullRequestEvent,
    number: i32,
) -> AppResult<Value> {
    let repo = ev.repository.full_name.as_str();
    let head = &ev.pull_request.head;
    if !is_sha(&head.sha) || validate_branch(&head.git_ref).is_err() {
        return Ok(json!({}));
    }
    let owner = repo.split_once('/').map_or(repo, |(o, _)| o);
    // Projects with previews whose base environment deploys this repository.
    let projects: Vec<(Id, Id, Id)> = sqlx::query_as(
        "WITH p AS (
             SELECT p.id, p.workspace_id,
                    coalesce(p.pr_base_environment_id,
                             (SELECT f.id FROM environments f WHERE f.project_id = p.id
                              ORDER BY f.created_at, f.id LIMIT 1)) AS base
             FROM projects p
             JOIN github_installations g ON g.workspace_id = p.workspace_id
             WHERE p.pr_previews AND g.id = $1 AND g.app_id = $2
               AND lower(g.account_login) = lower($3))
         SELECT p.id, p.workspace_id, p.base FROM p
         WHERE EXISTS (SELECT 1 FROM service_instances i WHERE i.environment_id = p.base
                         AND i.config->'source'->>'type' IN ('repo', 'function')
                         AND lower(i.config->'source'->>'repo') = lower($4))
         ORDER BY p.id",
    )
    .bind(installation)
    .bind(app)
    .bind(owner)
    .bind(repo)
    .fetch_all(&state.db)
    .await?;
    if projects.is_empty() {
        return Ok(json!({}));
    }
    let commit = Commit {
        sha: head.sha.clone(),
        message: ev.pull_request.title.chars().take(1000).collect(),
        author: ev.pull_request.user.login.chars().take(200).collect(),
        author_login: Some(ev.pull_request.user.login.clone()).filter(|l| super::valid_login(l)),
    };
    let mut changed: Option<Option<Vec<String>>> = None;
    let mut previews = Vec::new();
    let mut started = Vec::new();
    for (project, workspace, base) in projects {
        let existing: Option<Id> = sqlx::query_scalar(
            "SELECT id FROM environments
             WHERE project_id = $1 AND pr_number = $2 AND lower(pr_repo) = lower($3)",
        )
        .bind(project)
        .bind(number)
        .bind(repo)
        .fetch_optional(&state.db)
        .await?;
        let (env, created) = match existing {
            Some(env) => {
                sqlx::query("UPDATE environments SET pr_head_sha = $2 WHERE id = $1")
                    .bind(env)
                    .bind(&head.sha)
                    .execute(&state.db)
                    .await?;
                (env, false)
            }
            None => {
                match create(state, meta, project, workspace, base, repo, number, head).await? {
                    Some(env) => (env, true),
                    None => continue,
                }
            }
        };
        previews.push(env);
        let instances: Vec<(Id, serde_json::Value)> = sqlx::query_as(
            "SELECT i.id, i.config FROM service_instances i JOIN services s ON s.id = i.service_id
             WHERE i.environment_id = $1 ORDER BY s.created_at",
        )
        .bind(env)
        .fetch_all(&state.db)
        .await?;
        for (instance, config) in instances {
            let cfg: ServiceConfig = match serde_json::from_value(config) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(%instance, error = %e, "unreadable service config; not deployed");
                    continue;
                }
            };
            if !cfg.deployable() {
                continue;
            }
            let this_repo = cfg
                .repo()
                .is_some_and(|(r, _, _)| r.eq_ignore_ascii_case(repo));
            // A new preview starts everything; later pushes redeploy only this
            // repository's services their changes touch.
            let commit = if this_repo {
                if !created && !cfg.watch_paths.is_empty() {
                    if changed.is_none() {
                        changed = Some(changed_files(state, installation, repo, number).await);
                    }
                    let files = changed.as_ref().and_then(|c| c.as_ref());
                    let files: Option<Vec<&str>> =
                        files.map(|f| f.iter().map(String::as_str).collect());
                    if !should_deploy(&cfg.watch_paths, files.as_deref()) {
                        continue;
                    }
                }
                Some(commit.clone())
            } else if !created {
                continue;
            } else if let Some((other, branch, _)) = cfg.repo() {
                match app::branch_head(state, workspace, other, branch).await {
                    Ok(c) => Some(c),
                    Err(AppError::Internal(e)) => return Err(AppError::Internal(e)),
                    Err(e) => {
                        tracing::warn!(%instance, error = %e, "preview service not deployed");
                        continue;
                    }
                }
            } else {
                None
            };
            if let Some(id) = deploy(
                state,
                meta,
                delivery,
                instance,
                env,
                workspace,
                cfg,
                commit.as_ref(),
            )
            .await?
            {
                started.push(id);
            }
        }
        queue_comment(&state.db, project, repo, number, None).await?;
    }
    Ok(json!({ "previews": previews, "deployments": started }))
}

/// Duplicate `base` as the preview of pull request `number`, with a
/// generated domain for each service the base environment exposes. None
/// when the project has its fill of previews or the name is taken.
#[allow(clippy::too_many_arguments)]
async fn create(
    state: &AppState,
    meta: &RequestMeta,
    project: Id,
    workspace: Id,
    base: Id,
    repo: &str,
    number: i32,
    head: &Head,
) -> AppResult<Option<Id>> {
    let open: i64 =
        sqlx::query_scalar("SELECT count(*) FROM environments WHERE project_id = $1 AND ephemeral")
            .bind(project)
            .fetch_one(&state.db)
            .await?;
    if open >= MAX_PREVIEWS {
        tracing::warn!(%project, number, "project has {MAX_PREVIEWS} previews; no more");
        return Ok(None);
    }
    let services: Vec<Id> = sqlx::query_scalar(
        "SELECT service_id FROM service_instances
         WHERE environment_id = $1 AND config->'source'->>'type' IN ('repo', 'function')
           AND lower(config->'source'->>'repo') = lower($2)",
    )
    .bind(base)
    .bind(repo)
    .fetch_all(&state.db)
    .await?;
    let branches: BTreeMap<Id, String> = services
        .into_iter()
        .map(|s| (s, head.git_ref.clone()))
        .collect();
    let new = NewEnvironment {
        project,
        workspace,
        name: format!("pr-{number}"),
        duplicate_from: Some(base),
        branches,
        copy_sealed: false,
        pr: Some(PrRef {
            repo,
            number,
            head_sha: &head.sha,
        }),
    };
    let env = match projects::insert_environment(state, None, meta, new).await {
        Ok(env) => env,
        Err(AppError::Conflict(e)) => {
            tracing::warn!(%project, number, error = %e, "no preview environment");
            return Ok(None);
        }
        Err(e) => return Err(e),
    };
    if state.apps_domain().is_some() {
        let exposed: Vec<Id> = sqlx::query_scalar(
            "SELECT ti.id FROM service_instances ti
             JOIN service_instances bi ON bi.service_id = ti.service_id AND bi.environment_id = $2
             WHERE ti.environment_id = $1
               AND EXISTS (SELECT 1 FROM domains d WHERE d.service_instance_id = bi.id)",
        )
        .bind(env)
        .bind(base)
        .fetch_all(&state.db)
        .await?;
        for instance in exposed {
            if let Err(e) =
                crate::api::domains::generate_for(state, None, meta, workspace, instance).await
            {
                tracing::warn!(%instance, error = %e, "no domain for the preview");
            }
        }
    }
    Ok(Some(env))
}

/// The files pull request `number` changes; None when GitHub can't say.
async fn changed_files(
    state: &AppState,
    installation: i64,
    repo: &str,
    number: i32,
) -> Option<Vec<String>> {
    let token = app::installation_token(
        state,
        installation,
        TokenScope::PullRequests(repo.to_owned()),
    )
    .await
    .ok()?;
    #[derive(Deserialize)]
    struct File {
        filename: String,
        #[serde(default)]
        previous_filename: Option<String>,
    }
    let mut out = Vec::new();
    for page in 1..=MAX_FILE_PAGES {
        let resp = state
            .github
            .request(
                Method::GET,
                &format!("/repos/{repo}/pulls/{number}/files?per_page=100&page={page}"),
            )
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let files: Vec<File> = resp.json().await.ok()?;
        let n = files.len();
        out.extend(
            files
                .into_iter()
                .flat_map(|f| std::iter::once(f.filename).chain(f.previous_filename)),
        );
        if n < 100 {
            return Some(out);
        }
    }
    None
}

/// Delete the previews of a closed pull request and say so on it.
async fn close(
    state: &AppState,
    meta: &RequestMeta,
    app: i64,
    installation: i64,
    repo: &str,
    number: i32,
) -> AppResult<Value> {
    let envs: Vec<(Id, Id, Id, Option<i64>)> = sqlx::query_as(
        "SELECT e.id, e.project_id, p.workspace_id, e.pr_comment_id FROM environments e
         JOIN projects p ON p.id = e.project_id
         JOIN github_installations g ON g.workspace_id = p.workspace_id
         WHERE g.id = $1 AND g.app_id = $2 AND e.pr_number = $3 AND lower(e.pr_repo) = lower($4)",
    )
    .bind(installation)
    .bind(app)
    .bind(number)
    .bind(repo)
    .fetch_all(&state.db)
    .await?;
    let mut removed = Vec::new();
    for (env, project, workspace, comment) in envs {
        projects::remove_environment(state, None, meta, workspace, env).await?;
        // After the delete: the comment then reads "removed", whenever it runs.
        queue_comment(&state.db, project, repo, number, comment).await?;
        removed.push(env);
    }
    Ok(json!({ "removed": removed }))
}

/// Update the preview comment of `project` on pull request `number`.
/// `comment` is its id when the environment that remembered it is gone.
pub async fn queue_comment<'e>(
    db: impl sqlx::PgExecutor<'e>,
    project: Id,
    repo: &str,
    number: i32,
    comment: Option<i64>,
) -> AppResult<()> {
    jobs::enqueue(
        db,
        NewJob {
            kind: COMMENT_JOB,
            payload: json!({
                "projectId": project, "repo": repo, "number": number, "commentId": comment,
            }),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// Queue a comment update if `deployment` belongs to a preview.
pub async fn queue_comment_for(tx: &mut sqlx::PgConnection, deployment: Id) -> AppResult<()> {
    let pr: Option<(Id, String, i32)> = sqlx::query_as(
        "SELECT e.project_id, e.pr_repo, e.pr_number FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN environments e ON e.id = i.environment_id
         WHERE d.id = $1 AND e.pr_number IS NOT NULL",
    )
    .bind(deployment)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((project, repo, number)) = pr {
        queue_comment(&mut *tx, project, &repo, number, None).await?;
    }
    Ok(())
}

/// The hidden line that identifies a project's comment on a pull request.
fn marker(project: Id) -> String {
    format!("<!-- modula-preview:{project} -->")
}

/// What a deployment's status reads as in the comment.
fn status_label(status: Option<&str>) -> &'static str {
    match status {
        Some("waiting_ci") => "Waiting for CI",
        Some("queued") => "Queued",
        Some("building") => "Building",
        Some("deploying") => "Deploying",
        Some("active") => "Ready",
        Some("sleeping") => "Ready (sleeping)",
        Some("failed") => "Failed",
        Some("crashed") => "Crashed",
        Some("cancelled") => "Cancelled",
        Some("removed") => "Removed",
        _ => "Not deployed",
    }
}

/// Markdown-table-safe text.
fn cell(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, '|' | '<' | '>' | '\n' | '\r' | '`'))
        .take(100)
        .collect()
}

async fn render(
    state: &AppState,
    project: Id,
    project_name: &str,
    env: Option<Id>,
) -> AppResult<String> {
    let mut body = marker(project);
    body.push('\n');
    let name = cell(project_name);
    let Some(env) = env else {
        body.push_str(&format!(
            "The **{name}** preview was removed with the pull request."
        ));
        return Ok(body);
    };
    #[derive(sqlx::FromRow)]
    struct Row {
        service: String,
        status: Option<String>,
        url: Option<String>,
        hostname: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT s.name AS service, d.status, d.url,
                (SELECT hostname FROM domains WHERE service_instance_id = i.id AND status = 'verified'
                 ORDER BY created_at LIMIT 1) AS hostname
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         LEFT JOIN LATERAL (
             SELECT status, url FROM deployments WHERE service_instance_id = i.id
             ORDER BY created_at DESC LIMIT 1
         ) d ON true
         WHERE i.environment_id = $1 AND (d.status IS NOT NULL OR i.config->'source'->>'type' <> 'r2')
         ORDER BY s.created_at",
    )
    .bind(env)
    .fetch_all(&state.db)
    .await?;
    let mut link = state.public_url();
    link.set_path(&format!("/projects/{project}"));
    link.query_pairs_mut().append_pair("env", &env.to_string());
    body.push_str(&format!(
        "**{name}** preview ([open in Modula Deploy]({link}))\n\n| Service | URL | Status |\n| --- | --- | --- |\n"
    ));
    for r in rows {
        let url = r
            .hostname
            .map(|h| format!("https://{h}"))
            .or(r.url.filter(|u| u.starts_with("https://")))
            .map_or_else(|| "—".to_owned(), |u| cell(&u));
        body.push_str(&format!(
            "| {} | {url} | {} |\n",
            cell(&r.service),
            status_label(r.status.as_deref())
        ));
    }
    Ok(body)
}

/// Job: write the preview comment, editing the one already there.
pub async fn comment_job(state: AppState, job: Job) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        project_id: Id,
        repo: String,
        number: i32,
        comment_id: Option<i64>,
    }
    let p: Payload = serde_json::from_value(job.payload)?;
    validate_repo(&p.repo).map_err(|e| anyhow::anyhow!("{e}"))?;
    let project: Option<(String, Id)> =
        sqlx::query_as("SELECT name, workspace_id FROM projects WHERE id = $1")
            .bind(p.project_id)
            .fetch_optional(&state.db)
            .await?;
    let Some((project_name, workspace)) = project else {
        return Ok(());
    };
    let env: Option<(Id, Option<i64>)> = sqlx::query_as(
        "SELECT id, pr_comment_id FROM environments
         WHERE project_id = $1 AND pr_number = $2 AND lower(pr_repo) = lower($3)",
    )
    .bind(p.project_id)
    .bind(p.number)
    .bind(&p.repo)
    .fetch_optional(&state.db)
    .await?;
    let Some(installation) = app::installation_for(&state, workspace, &p.repo).await? else {
        return Ok(());
    };
    let token = app::installation_token(&state, installation, TokenScope::Issues(p.repo.clone()))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let token = token.expose_secret();
    let body = render(&state, p.project_id, &project_name, env.map(|e| e.0)).await?;
    let mut id = env.and_then(|e| e.1).or(p.comment_id);
    if id.is_none() {
        id = find_comment(&state, token, &p.repo, p.number, &marker(p.project_id)).await?;
    }
    if let Some(c) = id {
        let resp = state
            .github
            .request(
                Method::PATCH,
                &format!("/repos/{}/issues/comments/{c}", p.repo),
            )
            .bearer_auth(token)
            .json(&json!({ "body": body }))
            .send()
            .await?;
        match resp.status().as_u16() {
            200 => {}
            // Deleted on GitHub: post a new one.
            404 => id = None,
            s => anyhow::bail!("GitHub answered {s} to a comment update"),
        }
    }
    if id.is_none() {
        let resp = state
            .github
            .request(
                Method::POST,
                &format!("/repos/{}/issues/{}/comments", p.repo, p.number),
            )
            .bearer_auth(token)
            .json(&json!({ "body": body }))
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("GitHub answered {} to a new comment", resp.status());
        }
        #[derive(Deserialize)]
        struct Created {
            id: i64,
        }
        id = Some(resp.json::<Created>().await?.id);
    }
    if let Some((env, _)) = env {
        sqlx::query("UPDATE environments SET pr_comment_id = $2 WHERE id = $1")
            .bind(env)
            .bind(id)
            .execute(&state.db)
            .await?;
    }
    Ok(())
}

/// Our earlier comment on the pull request: posted by a bot (an App, so
/// nobody else's comment is ever edited) and carrying `marker`.
async fn find_comment(
    state: &AppState,
    token: &str,
    repo: &str,
    number: i32,
    marker: &str,
) -> anyhow::Result<Option<i64>> {
    #[derive(Deserialize)]
    struct Comment {
        id: i64,
        #[serde(default)]
        body: Option<String>,
        user: Option<CommentUser>,
    }
    #[derive(Deserialize)]
    struct CommentUser {
        #[serde(rename = "type")]
        kind: String,
    }
    for page in 1..=10 {
        let resp = state
            .github
            .request(
                Method::GET,
                &format!("/repos/{repo}/issues/{number}/comments?per_page=100&page={page}"),
            )
            .bearer_auth(token)
            .send()
            .await?
            .error_for_status()?;
        let comments: Vec<Comment> = resp.json().await?;
        let n = comments.len();
        if let Some(c) = comments.into_iter().find(|c| {
            c.user.as_ref().is_some_and(|u| u.kind == "Bot")
                && c.body.as_deref().is_some_and(|b| b.starts_with(marker))
        }) {
            return Ok(Some(c.id));
        }
        if n < 100 {
            break;
        }
    }
    Ok(None)
}
