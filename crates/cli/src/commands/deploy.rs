//! Deploying: `deploy`, `redeploy`, `restart`, `down`, `rollback` and
//! `deployment list` (`up`, which uploads local files, is in [`super::up`]).

use std::time::{Duration, Instant};

use crate::api::{Deployed, Deployment, Environment};
use crate::cli::DeploymentCommand;
use crate::commands::logs;
use crate::context::Ctx;
use crate::output::{Table, or_dash, short_time, truncate};

const POLL: Duration = Duration::from_secs(1);
/// Give up waiting after this long (the deployment carries on).
const WAIT_LIMIT: Duration = Duration::from_secs(60 * 60);

/// The newest deployment whose status is one of `statuses`.
pub fn latest_with<'a>(deployments: &'a [Deployment], statuses: &[&str]) -> Option<&'a Deployment> {
    deployments
        .iter()
        .find(|d| statuses.contains(&d.status.as_str()))
}

/// Statuses a deployment can be removed in.
const LIVE: &[&str] = &["queued", "building", "deploying", "active", "crashed"];
/// Statuses a deployment can be restarted in.
const RUNNING: &[&str] = &["active", "crashed"];

/// Poll until the deployment settles; prints status changes (and, with
/// `build_logs`, streams the build output while it builds).
pub async fn wait(ctx: &Ctx, id: &str, build_logs: bool) -> anyhow::Result<Deployment> {
    let api = ctx.api()?;
    let started = Instant::now();
    let mut last = String::new();
    let mut streamed = false;
    loop {
        let d = api.deployment(id).await?;
        if d.status != last {
            ctx.out.note(format!("  {}", d.status));
            last.clone_from(&d.status);
        }
        if d.is_final() {
            return Ok(d);
        }
        if build_logs && !streamed && d.status == "building" {
            streamed = true;
            let opts = crate::api::endpoints::LogOptions {
                tail: 5000,
                follow: true,
                build: true,
            };
            // Logs are a bonus here; the status poll decides the outcome.
            if let Err(e) = logs::print_stream(ctx, id, opts).await {
                ctx.out.note(format!("  (build logs unavailable: {e:#})"));
            }
            continue;
        }
        if started.elapsed() > WAIT_LIMIT {
            anyhow::bail!("gave up waiting for deployment {id} (still {})", d.status);
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Exit code for a settled deployment, after saying how it went.
pub fn outcome(ctx: &Ctx, d: &Deployment) -> i32 {
    if d.status == "active" {
        ctx.out.note(format!("Deployment {} is live.", d.id));
        return 0;
    }
    ctx.out.note(format!(
        "Deployment {} {}{}",
        d.id,
        d.status,
        d.error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    ));
    1
}

/// Deploy `services` (or every service with pending changes) in `env`.
pub async fn deploy_environment(
    ctx: &Ctx,
    env: &Environment,
    services: Option<Vec<String>>,
    wait_for: bool,
) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    let result = api.deploy_environment(&env.id, services.as_deref()).await?;
    let mut code = i32::from(result.deployments.iter().any(|d| d.error.is_some()));
    let mut settled = Vec::new();
    if wait_for {
        for d in result
            .deployments
            .iter()
            .filter_map(|d| d.deployment_id.as_deref())
        {
            let d = wait(ctx, d, false).await?;
            code = code.max(outcome(ctx, &d));
            settled.push(d);
        }
    }
    let names = service_names(ctx, &result.deployments).await;
    let body = serde_json::json!({ "deployments": result.deployments, "settled": settled });
    ctx.out.either(&body, |o| {
        if result.deployments.is_empty() {
            o.line(format!(
                "Nothing pending in {}; pass --service to deploy one anyway.",
                env.name
            ));
            return;
        }
        let mut t = Table::new(&["service", "deployment", "error"]);
        for (d, name) in result.deployments.iter().zip(&names) {
            t.row([
                name.clone(),
                or_dash(d.deployment_id.as_deref()),
                or_dash(d.error.as_deref()),
            ]);
        }
        o.table(&t);
    })?;
    Ok(code)
}

/// Service names for deploy results (ids when the project can't be read).
async fn service_names(ctx: &Ctx, deployed: &[Deployed]) -> Vec<String> {
    let project = ctx.project().await.ok();
    deployed
        .iter()
        .map(|d| {
            project
                .as_ref()
                .and_then(|p| p.services.iter().find(|s| s.id == d.service_id))
                .map_or_else(|| d.service_id.clone(), |s| s.name.clone())
        })
        .collect()
}

/// `deploy`: the selected service, or every service with pending changes.
pub async fn deploy(ctx: &Ctx, all: bool, wait_for: bool) -> anyhow::Result<i32> {
    let p = ctx.project().await?;
    let env = ctx.environment(&p)?.clone();
    let services = match ctx.wanted_service(&p) {
        Some(w) if !all => Some(vec![
            crate::resolve::pick_service(&p.services, Some(w))?
                .id
                .clone(),
        ]),
        _ => None,
    };
    deploy_environment(ctx, &env, services, wait_for).await
}

/// `redeploy`, `restart`: act on the newest deployment in `statuses`.
pub async fn rollout(ctx: &Ctx, action: &str, wait_for: bool) -> anyhow::Result<i32> {
    let t = ctx.target().await?;
    let api = ctx.api()?;
    let deployments = api.deployments(&t.instance).await?;
    // Restart needs a running container; redeploy repeats the newest.
    let latest = match action {
        "restart" => latest_with(&deployments, RUNNING),
        _ => deployments.first(),
    };
    let Some(latest) = latest else {
        anyhow::bail!(
            "{} has no deployment to {action}{}",
            t.label(),
            if action == "restart" {
                " (nothing is running)"
            } else {
                ""
            }
        );
    };
    let d = api.rollout(&latest.id, action).await?;
    let mut code = 0;
    let d = if wait_for {
        let d = wait(ctx, &d.id, false).await?;
        code = outcome(ctx, &d);
        d
    } else {
        d
    };
    ctx.out.either(&d, |o| {
        let verb = if action == "restart" {
            "Restarted"
        } else {
            "Redeploying"
        };
        o.line(format!("{verb} {}: deployment {}", t.label(), d.id));
    })?;
    Ok(code)
}

pub async fn down(ctx: &Ctx) -> anyhow::Result<i32> {
    let t = ctx.target().await?;
    let api = ctx.api()?;
    let deployments = api.deployments(&t.instance).await?;
    let Some(latest) = latest_with(&deployments, LIVE) else {
        anyhow::bail!("{} has no running deployment", t.label());
    };
    ctx.confirm(&format!(
        "Stop and remove deployment {} of {}?",
        latest.id,
        t.label()
    ))?;
    let d = api.remove_deployment(&latest.id).await?;
    ctx.out.either(&d, |o| {
        o.line(format!("Removed deployment {} of {}", d.id, t.label()))
    })?;
    Ok(0)
}

pub async fn rollback(ctx: &Ctx, deployment: &str, wait_for: bool) -> anyhow::Result<i32> {
    let api = ctx.api()?;
    let from = api.deployment(deployment).await?;
    let commit = from
        .commit_sha
        .as_deref()
        .map(|s| s.chars().take(7).collect::<String>())
        .unwrap_or_else(|| "-".into());
    ctx.confirm(&format!(
        "Roll back to deployment {} ({}, commit {commit})? It is rebuilt and replaces what's live.",
        from.id, from.source
    ))?;
    let d = api.rollout(deployment, "rollback").await?;
    let mut code = 0;
    let d = if wait_for {
        let d = wait(ctx, &d.id, false).await?;
        code = outcome(ctx, &d);
        d
    } else {
        d
    };
    ctx.out
        .either(&d, |o| o.line(format!("Rolling back: deployment {}", d.id)))?;
    Ok(code)
}

pub async fn deployment(ctx: &Ctx, command: DeploymentCommand) -> anyhow::Result<i32> {
    let DeploymentCommand::List { limit } = command;
    let t = ctx.target().await?;
    let mut deployments = ctx.api()?.deployments(&t.instance).await?;
    deployments.truncate(limit);
    ctx.out.either(&deployments, |o| {
        if deployments.is_empty() {
            o.line(format!("{} has no deployments yet.", t.label()));
            return;
        }
        o.table(&deployment_table(&deployments));
    })?;
    Ok(0)
}

pub fn deployment_table(deployments: &[Deployment]) -> Table {
    let mut t = Table::new(&["id", "status", "created", "source", "commit"]).status_column(1);
    for d in deployments {
        let commit = match (&d.commit_sha, &d.commit_message) {
            (Some(sha), msg) => format!(
                "{} {}",
                sha.chars().take(7).collect::<String>(),
                truncate(msg.as_deref().unwrap_or(""), 40)
            ),
            _ => "-".into(),
        };
        t.row([
            d.id.clone(),
            d.status.clone(),
            short_time(&d.created_at),
            or_dash(Some(&d.source)),
            commit.trim_end().to_owned(),
        ]);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(id: &str, status: &str) -> Deployment {
        serde_json::from_value(serde_json::json!({
            "id": id, "status": status, "source": "nginx:alpine",
            "createdAt": "2026-10-23T10:00:00Z", "image": "nginx:alpine"
        }))
        .unwrap()
    }

    #[test]
    fn the_right_latest_deployment() {
        let ds = [
            dep("d3", "failed"),
            dep("d2", "active"),
            dep("d1", "removed"),
        ];
        assert_eq!(latest_with(&ds, RUNNING).unwrap().id, "d2");
        assert_eq!(latest_with(&ds, LIVE).unwrap().id, "d2");
        assert!(latest_with(&ds[..1], RUNNING).is_none());
        // Unknown fields survive for --json.
        assert_eq!(ds[0].rest["image"], "nginx:alpine");
        assert!(ds[0].is_final() && !dep("x", "building").is_final());
    }

    #[test]
    fn deployment_rows() {
        let mut d = dep("d1", "active");
        d.commit_sha = Some("0123456789abcdef".into());
        d.commit_message = Some("Fix the thing\n\nbody".into());
        let text = deployment_table(&[d]).render(false);
        assert!(text.contains("0123456 Fix the thing…"), "{text}");
        assert!(text.contains("2026-10-23 10:00"), "{text}");
    }
}
