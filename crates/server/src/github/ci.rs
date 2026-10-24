//! Wait for CI (`waitForCi`): a push deployment held at `waiting_ci` until
//! every other check on its commit has passed. `check_suite` and `status`
//! webhooks re-evaluate the commit; the verdict comes from GitHub's API, not
//! the event, so the order deliveries arrive in doesn't matter. Our own
//! `modula/*` statuses and our own App's check suites never count.

use modula_id::Id;
use reqwest::Method;
use secrecy::ExposeSecret;
use serde::Deserialize;
use time::{Duration, OffsetDateTime};

use super::app::{self, TokenScope};
use super::push::InstallationRef;
use crate::error::{AppError, AppResult};
use crate::jobs::{self, Job, NewJob};
use crate::state::AppState;

/// How long a deployment waits for CI before it fails.
pub const TIMEOUT: Duration = Duration::hours(2);
pub const TIMEOUT_JOB: &str = "github.ci_timeout";
/// Our own commit statuses (`app::status_job`).
const OWN_CONTEXT: &str = "modula/";

/// Fail `deployment` if it still waits for CI after [`TIMEOUT`].
pub async fn schedule_timeout(tx: &mut sqlx::PgConnection, deployment: Id) -> AppResult<()> {
    let key = format!("ci_timeout:{deployment}");
    jobs::enqueue(
        &mut *tx,
        NewJob {
            kind: TIMEOUT_JOB,
            payload: serde_json::json!({ "deploymentId": deployment }),
            run_at: Some(OffsetDateTime::now_utc() + TIMEOUT),
            dedupe_key: Some(&key),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// Job: the deployment's CI didn't finish in time.
pub async fn timeout_job(state: AppState, job: Job) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        deployment_id: Id,
    }
    let p: Payload = serde_json::from_value(job.payload)?;
    let hours = TIMEOUT.whole_hours();
    fail(
        &state,
        p.deployment_id,
        &format!("CI didn't finish within {hours} hours"),
    )
    .await?;
    Ok(())
}

/// What the commit's checks say.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Something is still running, or nothing has reported yet.
    Pending,
    Passed,
    /// The first failing check, for the deployment's error.
    Failed(String),
}

#[derive(Deserialize)]
pub struct CheckSuiteEvent {
    action: String,
    check_suite: Suite,
    repository: Repository,
    installation: Option<InstallationRef>,
}

#[derive(Deserialize)]
pub struct StatusEvent {
    sha: String,
    context: String,
    state: String,
    repository: Repository,
    installation: Option<InstallationRef>,
}

#[derive(Deserialize)]
struct Repository {
    full_name: String,
}

#[derive(Deserialize)]
struct Suite {
    #[serde(default)]
    head_sha: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    app: Option<SuiteApp>,
    #[serde(default)]
    latest_check_runs_count: Option<i64>,
}

#[derive(Deserialize)]
struct SuiteApp {
    id: i64,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct Status {
    context: String,
    state: String,
}

pub async fn handle_check_suite(
    state: &AppState,
    app: i64,
    ev: &CheckSuiteEvent,
) -> AppResult<Vec<Id>> {
    if ev.action != "completed" {
        return Ok(vec![]);
    }
    let Some(installation) = &ev.installation else {
        return Ok(vec![]);
    };
    settle(
        state,
        app,
        installation.id,
        &ev.repository.full_name,
        &ev.check_suite.head_sha,
    )
    .await
}

pub async fn handle_status(state: &AppState, app: i64, ev: &StatusEvent) -> AppResult<Vec<Id>> {
    // Our own statuses come back as events too.
    if ev.context.starts_with(OWN_CONTEXT) || ev.state == "pending" {
        return Ok(vec![]);
    }
    let Some(installation) = &ev.installation else {
        return Ok(vec![]);
    };
    settle(
        state,
        app,
        installation.id,
        &ev.repository.full_name,
        &ev.sha,
    )
    .await
}

/// Release or fail every deployment of `repo` at `sha` that waits for CI,
/// in a workspace with `installation` of `app`. Returns the deployments
/// released or failed.
async fn settle(
    state: &AppState,
    app: i64,
    installation: i64,
    repo: &str,
    sha: &str,
) -> AppResult<Vec<Id>> {
    if !super::push::is_sha(sha) || crate::deploy::config::validate_repo(repo).is_err() {
        return Ok(vec![]);
    }
    let waiting: Vec<Id> = sqlx::query_scalar(
        "SELECT d.id FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         JOIN github_installations g ON g.workspace_id = p.workspace_id
         WHERE d.status = 'waiting_ci' AND d.commit_sha = $1
           AND lower(d.config->'source'->>'repo') = lower($2)
           AND g.id = $3 AND g.app_id = $4
         ORDER BY d.created_at",
    )
    .bind(sha)
    .bind(repo)
    .bind(installation)
    .bind(app)
    .fetch_all(&state.db)
    .await?;
    if waiting.is_empty() {
        return Ok(vec![]);
    }
    let verdict = evaluate(state, app, installation, repo, sha).await?;
    let mut settled = Vec::new();
    for dep in waiting {
        let done = match &verdict {
            Verdict::Pending => false,
            Verdict::Passed => release(state, dep).await?,
            Verdict::Failed(why) => fail(state, dep, why).await?,
        };
        if done {
            settled.push(dep);
        }
    }
    Ok(settled)
}

/// Read the commit's check suites and statuses as `installation`.
pub async fn evaluate(
    state: &AppState,
    app: i64,
    installation: i64,
    repo: &str,
    sha: &str,
) -> AppResult<Verdict> {
    let token =
        app::installation_token(state, installation, TokenScope::Checks(repo.to_owned())).await?;
    let get = |path: String| {
        state
            .github
            .request(Method::GET, &path)
            .bearer_auth(token.expose_secret())
            .send()
    };
    #[derive(Deserialize)]
    struct Suites {
        check_suites: Vec<Suite>,
    }
    #[derive(Deserialize)]
    struct Combined {
        #[serde(default)]
        statuses: Vec<Status>,
    }
    let resp = get(format!(
        "/repos/{repo}/commits/{sha}/check-suites?per_page=100"
    ))
    .await
    .map_err(unreachable)?;
    if !resp.status().is_success() {
        return Err(unexpected(resp.status().as_u16()));
    }
    let suites: Suites = resp.json().await.map_err(anyhow::Error::from)?;
    let resp = get(format!("/repos/{repo}/commits/{sha}/status?per_page=100"))
        .await
        .map_err(unreachable)?;
    if !resp.status().is_success() {
        return Err(unexpected(resp.status().as_u16()));
    }
    let combined: Combined = resp.json().await.map_err(anyhow::Error::from)?;
    Ok(verdict(app, &suites.check_suites, &combined.statuses))
}

fn unreachable(e: reqwest::Error) -> AppError {
    tracing::warn!(error = %e, "GitHub API unreachable");
    AppError::Unavailable("GitHub could not be reached; try again".into())
}

fn unexpected(status: u16) -> AppError {
    tracing::warn!(status, "unexpected GitHub API response reading CI");
    AppError::Unavailable("GitHub returned an error; try again".into())
}

/// Every suite (but our own App's, and suites no App ever ran anything in:
/// GitHub creates one per installed App with checks access, and those stay
/// queued forever) and every status (but our own) must have succeeded.
fn verdict(app: i64, suites: &[Suite], statuses: &[Status]) -> Verdict {
    let mut pending = false;
    let mut seen = false;
    for s in suites {
        if s.app.as_ref().is_some_and(|a| a.id == app) || s.latest_check_runs_count == Some(0) {
            continue;
        }
        seen = true;
        let name = s
            .app
            .as_ref()
            .and_then(|a| a.name.clone())
            .unwrap_or_else(|| "a check suite".into());
        match (s.status.as_deref(), s.conclusion.as_deref()) {
            (Some("completed"), Some("success" | "neutral" | "skipped")) => {}
            (Some("completed"), c) => {
                return Verdict::Failed(format!(
                    "CI failed: {name} concluded {}",
                    c.unwrap_or("without a result")
                ));
            }
            _ => pending = true,
        }
    }
    for s in statuses {
        if s.context.starts_with(OWN_CONTEXT) {
            continue;
        }
        seen = true;
        match s.state.as_str() {
            "success" => {}
            "pending" => pending = true,
            other => {
                let context: String = s.context.chars().take(200).collect();
                return Verdict::Failed(format!("CI failed: {context} reported {other}"));
            }
        }
    }
    if pending || !seen {
        Verdict::Pending
    } else {
        Verdict::Passed
    }
}

/// Start building a deployment that waited for CI. False when it no longer
/// waits (superseded, removed, timed out).
pub async fn release(state: &AppState, deployment: Id) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    let released: Option<Id> = sqlx::query_scalar(
        "UPDATE deployments SET status = 'building', updated_at = now()
         WHERE id = $1 AND status = 'waiting_ci' RETURNING id",
    )
    .bind(deployment)
    .fetch_optional(&mut *tx)
    .await?;
    if released.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message)
         VALUES ($1, 'queued', 'CI passed: building')",
    )
    .bind(deployment)
    .execute(&mut *tx)
    .await?;
    let nodes: Vec<Id> = sqlx::query_scalar(
        "SELECT node_id FROM deployment_replicas WHERE deployment_id = $1 AND node_id IS NOT NULL",
    )
    .bind(deployment)
    .fetch_all(&mut *tx)
    .await?;
    for n in &nodes {
        crate::deploy::bump(&mut *tx, *n).await?;
    }
    tx.commit().await?;
    state.hub.kick_all(&nodes);
    Ok(true)
}

/// Fail a deployment that waits for CI with `reason`. False when it no
/// longer waits.
pub async fn fail(state: &AppState, deployment: Id, reason: &str) -> AppResult<bool> {
    let mut tx = state.db.begin().await?;
    let failed: Option<Id> = sqlx::query_scalar(
        "UPDATE deployments SET status = 'failed', error = $2, finished_at = now(), updated_at = now()
         WHERE id = $1 AND status = 'waiting_ci' RETURNING id",
    )
    .bind(deployment)
    .bind(reason)
    .fetch_optional(&mut *tx)
    .await?;
    if failed.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO deployment_events (deployment_id, phase, message) VALUES ($1, 'failed', $2)",
    )
    .bind(deployment)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    crate::webhooks::enqueue(
        &mut tx,
        deployment,
        crate::webhooks::Event::DeploymentFailed,
    )
    .await?;
    super::report(&mut tx, deployment, "failure", true).await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suite(app: i64, status: &str, conclusion: Option<&str>, runs: i64) -> Suite {
        Suite {
            head_sha: String::new(),
            status: Some(status.into()),
            conclusion: conclusion.map(Into::into),
            app: Some(SuiteApp {
                id: app,
                name: Some(format!("app{app}")),
            }),
            latest_check_runs_count: Some(runs),
        }
    }

    fn status(context: &str, state: &str) -> Status {
        Status {
            context: context.into(),
            state: state.into(),
        }
    }

    #[test]
    fn verdicts() {
        let ok = suite(1, "completed", Some("success"), 3);
        // Nothing reported yet: keep waiting.
        assert_eq!(verdict(9, &[], &[]), Verdict::Pending);
        assert_eq!(verdict(9, &[ok], &[]), Verdict::Passed);
        // Our own App's suite, empty suites and our own statuses don't count.
        let own = suite(9, "queued", None, 1);
        let empty = suite(2, "queued", None, 0);
        assert_eq!(
            verdict(
                9,
                &[suite(1, "completed", Some("neutral"), 1), own, empty],
                &[status("modula/api", "pending"), status("ci/x", "success")]
            ),
            Verdict::Passed
        );
        assert_eq!(
            verdict(9, &[suite(1, "in_progress", None, 1)], &[]),
            Verdict::Pending
        );
        assert_eq!(
            verdict(9, &[], &[status("ci/x", "pending")]),
            Verdict::Pending
        );
        assert!(matches!(
            verdict(9, &[suite(1, "completed", Some("failure"), 1)], &[]),
            Verdict::Failed(_)
        ));
        assert!(matches!(
            verdict(9, &[], &[status("ci/x", "error")]),
            Verdict::Failed(_)
        ));
        // Only our own pending status: nothing else to wait on yet.
        assert_eq!(
            verdict(9, &[], &[status("modula/api", "failure")]),
            Verdict::Pending
        );
    }
}
