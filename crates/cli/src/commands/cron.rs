//! `cron list` / `cron run`: a cron service's schedule and runs (the
//! dashboard's Cron tab).

use serde::{Deserialize, Serialize};

use crate::cli::CronCommand;
use crate::context::{Ctx, Target};
use crate::output::{Table, or_dash, short_time, truncate};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub expression: String,
    pub description: String,
    pub upcoming: Vec<String>,
    pub deployed: bool,
    pub timeout_secs: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub trigger: String,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub exit_code: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CronRuns {
    pub schedule: Option<Schedule>,
    pub runs: Vec<Run>,
}

pub async fn run(ctx: &Ctx, command: CronCommand) -> anyhow::Result<i32> {
    match command {
        CronCommand::List { service } => list(ctx, service.as_deref()).await,
        CronCommand::Run { service } => run_now(ctx, service.as_deref()).await,
    }
}

async fn target(ctx: &Ctx, service: Option<&str>) -> anyhow::Result<Target> {
    match service {
        Some(name) => {
            let project = ctx.project().await?;
            let s = crate::resolve::default_service(&project.services, Some(name))?.clone();
            ctx.target_with(project, s)
        }
        None => ctx.target().await,
    }
}

async fn list(ctx: &Ctx, service: Option<&str>) -> anyhow::Result<i32> {
    let t = target(ctx, service).await?;
    let r: CronRuns = ctx
        .api()?
        .get(&format!("/api/service-instances/{}/cron-runs", t.instance))
        .await?;
    ctx.out.either(&r, |o| {
        let Some(s) = &r.schedule else {
            o.line(format!("{} has no cron schedule.", t.label()));
            return;
        };
        o.line(format!(
            "{} · {} ({})",
            t.label(),
            s.description,
            s.expression
        ));
        if !s.deployed {
            o.line(o.dim("Not deployed yet: deploy to start the schedule."));
        }
        for at in &s.upcoming {
            o.line(format!("  next  {} UTC", short_time(at)));
        }
        o.line(String::new());
        if r.runs.is_empty() {
            o.line("No runs yet.");
            return;
        }
        let mut table =
            Table::new(&["id", "status", "trigger", "started", "result"]).status_column(1);
        for run in &r.runs {
            table.row([
                run.id.clone(),
                run.status.clone(),
                run.trigger.clone(),
                run.started_at
                    .as_deref()
                    .map_or_else(|| "-".into(), short_time),
                result(run),
            ]);
        }
        o.table(&table);
    })?;
    Ok(0)
}

fn result(run: &Run) -> String {
    match (&run.error, run.exit_code) {
        (Some(e), _) => truncate(e, 60),
        (None, Some(code)) => format!("exit {code}"),
        (None, None) => or_dash(None),
    }
}

async fn run_now(ctx: &Ctx, service: Option<&str>) -> anyhow::Result<i32> {
    let t = target(ctx, service).await?;
    let run: Run = ctx
        .api()?
        .post_empty(&format!("/api/service-instances/{}/cron-runs", t.instance))
        .await?;
    ctx.out.either(&run, |o| {
        o.line(format!(
            "Started a run of {}: {} ({})",
            t.label(),
            run.id,
            o.status(&run.status)
        ));
        if run.status == "failed"
            && let Some(e) = &run.error
        {
            o.line(e);
        }
    })?;
    Ok(if run.status == "failed" { 1 } else { 0 })
}
