//! `backup list` / `create` / `schedule` / `destinations`: a database
//! service's backups (the dashboard's Backups tab). Restoring needs a recent
//! sign-in the server refuses API tokens for, so it stays in the dashboard.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cli::{BackupCommand, BackupDest, BackupScheduleArgs};
use crate::context::{Ctx, Target};
use crate::output::{Table, or_dash, short_time, truncate, usage};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Retention {
    pub daily_days: i32,
    pub weekly_days: i32,
    pub monthly_days: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub enabled: bool,
    pub target_id: String,
    pub target_name: String,
    pub target_kind: String,
    pub hour_utc: i16,
    pub retention: Retention,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Backup {
    pub id: String,
    pub status: String,
    pub kind: String,
    pub size_bytes: Option<i64>,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub error: Option<String>,
    pub target_name: String,
    pub target_kind: String,
    pub method: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceBackups {
    pub schedule: Option<Schedule>,
    pub backups: Vec<Backup>,
    /// Restores are shown as the API returns them.
    #[serde(default)]
    pub restores: Vec<Value>,
}

pub async fn run(ctx: &Ctx, command: BackupCommand) -> anyhow::Result<i32> {
    match command {
        BackupCommand::List { service } => list(ctx, service.as_deref()).await,
        BackupCommand::Create {
            service,
            dest,
            no_wait,
        } => create(ctx, service.as_deref(), &dest, no_wait).await,
        BackupCommand::Schedule(args) => schedule(ctx, args).await,
        BackupCommand::Destinations { service } => destinations(ctx, service.as_deref()).await,
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

async fn fetch(ctx: &Ctx, instance: &str) -> anyhow::Result<InstanceBackups> {
    ctx.api()?
        .get(&format!("/api/service-instances/{instance}/backups"))
        .await
}

/// `1.4 GB`, `312 MB`, `87 KB`.
fn size(bytes: Option<i64>) -> String {
    let Some(b) = bytes else {
        return or_dash(None);
    };
    let b = b.max(0) as f64;
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut i = 0;
    let mut v = b;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 || v >= 100.0 {
        format!("{v:.0} {}", UNITS[i])
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// `disk`, `bucket` or `s3`, with the target's name.
fn place(kind: &str, name: &str) -> String {
    match kind {
        "local" => name.to_owned(),
        _ => format!("{name} ({kind})"),
    }
}

fn schedule_line(s: &Schedule) -> String {
    format!(
        "{} at {:02}:00 UTC to {} · keeps {} daily / {} weekly / {} monthly days",
        if s.enabled {
            "Daily"
        } else {
            "Disabled: daily"
        },
        s.hour_utc,
        place(&s.target_kind, &s.target_name),
        s.retention.daily_days,
        s.retention.weekly_days,
        s.retention.monthly_days,
    )
}

async fn list(ctx: &Ctx, service: Option<&str>) -> anyhow::Result<i32> {
    let t = target(ctx, service).await?;
    let r = fetch(ctx, &t.instance).await?;
    ctx.out.either(&r, |o| {
        match &r.schedule {
            Some(s) => o.line(format!("{} · {}", t.label(), schedule_line(s))),
            None => o.line(format!(
                "{} has no backup schedule (`modula backup schedule --hour 21 --local-disk`).",
                t.label()
            )),
        }
        o.line(String::new());
        if r.backups.is_empty() {
            o.line("No backups yet.");
            return;
        }
        let mut table = Table::new(&["id", "status", "kind", "started", "size", "stored", "error"])
            .status_column(1);
        for b in &r.backups {
            table.row([
                b.id.clone(),
                b.status.clone(),
                if b.method == "walg" {
                    "base".to_owned()
                } else {
                    b.kind.clone()
                },
                short_time(&b.started_at),
                size(b.size_bytes),
                place(&b.target_kind, &b.target_name),
                b.error
                    .as_deref()
                    .map_or_else(String::new, |e| truncate(e, 50)),
            ]);
        }
        o.table(&table);
    })?;
    Ok(0)
}

/// The API's destination fields; `None` when no flag picked one.
fn destination(dest: &BackupDest) -> Option<Value> {
    if dest.local_disk {
        Some(json!({ "localDisk": true }))
    } else if let Some(id) = &dest.target {
        Some(json!({ "targetId": id }))
    } else {
        dest.bucket
            .as_ref()
            .map(|id| json!({ "bucketInstanceId": id }))
    }
}

async fn create(
    ctx: &Ctx,
    service: Option<&str>,
    dest: &BackupDest,
    no_wait: bool,
) -> anyhow::Result<i32> {
    let t = target(ctx, service).await?;
    let path = format!("/api/service-instances/{}/backups", t.instance);
    let mut backup: Backup = match destination(dest) {
        Some(body) => ctx.api()?.post(&path, &body).await?,
        None => ctx.api()?.post_empty(&path).await?,
    };
    if !no_wait {
        if !ctx.out.json {
            eprintln!(
                "Backing up {} to {} ({})…",
                t.label(),
                place(&backup.target_kind, &backup.target_name),
                backup.id
            );
        }
        // Dumps of large databases take a while: wait up to two hours.
        for _ in 0..3600 {
            if backup.status != "running" {
                break;
            }
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                _ = tokio::signal::ctrl_c() => break,
            }
            if let Some(b) = fetch(ctx, &t.instance)
                .await?
                .backups
                .into_iter()
                .find(|b| b.id == backup.id)
            {
                backup = b;
            }
        }
    }
    ctx.out.either(&backup, |o| match backup.status.as_str() {
        "completed" => o.line(format!(
            "Backup {} completed: {} on {}",
            backup.id,
            size(backup.size_bytes),
            place(&backup.target_kind, &backup.target_name)
        )),
        "failed" => o.line(format!(
            "Backup {} failed: {}",
            backup.id,
            backup.error.as_deref().unwrap_or("no reason given")
        )),
        s => o.line(format!(
            "Backup {} is {} (`modula backup list` shows it).",
            backup.id,
            o.status(s)
        )),
    })?;
    Ok(if backup.status == "failed" { 1 } else { 0 })
}

/// The schedule to PUT: the flags over the current schedule (so `--hour 3`
/// alone keeps the destination and retention).
fn merged(current: Option<&Schedule>, args: &BackupScheduleArgs) -> anyhow::Result<Value> {
    let mut body = match destination(&args.dest) {
        Some(d) => d,
        None => match current {
            Some(s) => json!({ "targetId": s.target_id }),
            None => {
                return Err(usage(
                    "say where backups go: --local-disk, --target <id> or --bucket <instance id> \
                     (`modula backup destinations` lists them)",
                ));
            }
        },
    };
    let hour = match (args.hour, current) {
        (Some(h), _) => i16::from(h),
        (None, Some(s)) => s.hour_utc,
        (None, None) => return Err(usage("give the hour to back up at: --hour <0-23> (UTC)")),
    };
    let keep = |flag: Option<u16>, current: Option<i32>, default: i32| {
        flag.map(i32::from).or(current).unwrap_or(default)
    };
    let r = current.map(|s| &s.retention);
    body["hourUtc"] = json!(hour);
    body["enabled"] = json!(if args.enable {
        true
    } else if args.disable {
        false
    } else {
        // Setting a schedule turns it on unless it was switched off.
        current.is_none_or(|s| s.enabled)
    });
    body["retention"] = json!({
        "dailyDays": keep(args.daily, r.map(|r| r.daily_days), 6),
        "weeklyDays": keep(args.weekly, r.map(|r| r.weekly_days), 27),
        "monthlyDays": keep(args.monthly, r.map(|r| r.monthly_days), 89),
    });
    Ok(body)
}

fn changes(args: &BackupScheduleArgs) -> bool {
    args.hour.is_some()
        || destination(&args.dest).is_some()
        || args.daily.is_some()
        || args.weekly.is_some()
        || args.monthly.is_some()
        || args.enable
        || args.disable
}

async fn schedule(ctx: &Ctx, args: BackupScheduleArgs) -> anyhow::Result<i32> {
    let t = target(ctx, args.service.as_deref()).await?;
    let mut r = fetch(ctx, &t.instance).await?;
    if changes(&args) {
        let body = merged(r.schedule.as_ref(), &args)?;
        r = ctx
            .api()?
            .put(
                &format!("/api/service-instances/{}/backup-schedule", t.instance),
                &body,
            )
            .await?;
    }
    ctx.out.either(&r.schedule, |o| match &r.schedule {
        Some(s) => o.line(format!("{} · {}", t.label(), schedule_line(s))),
        None => o.line(format!("{} has no backup schedule.", t.label())),
    })?;
    Ok(0)
}

async fn destinations(ctx: &Ctx, service: Option<&str>) -> anyhow::Result<i32> {
    let t = target(ctx, service).await?;
    let d: Value = ctx
        .api()?
        .get(&format!(
            "/api/service-instances/{}/backup-destinations",
            t.instance
        ))
        .await?;
    ctx.out.either(&d, |o| {
        let text = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_owned();
        let mut table = Table::new(&["flag", "where"]);
        for s in d["servers"].as_array().into_iter().flatten() {
            table.row([
                "--local-disk".to_owned(),
                format!("Disk of {}", text(s, "name")),
            ]);
        }
        for b in d["buckets"].as_array().into_iter().flatten() {
            table.row([
                format!("--bucket {}", text(b, "instanceId")),
                format!(
                    "{} bucket {} ({} / {})",
                    text(b, "template"),
                    text(b, "serviceName"),
                    text(b, "projectName"),
                    text(b, "environmentName")
                ),
            ]);
        }
        for s3 in d["targets"].as_array().into_iter().flatten() {
            // Disk and bucket targets are the rows above once in use.
            if s3["kind"] == "s3" {
                table.row([
                    format!("--target {}", text(s3, "id")),
                    format!("{} ({})", text(s3, "name"), text(s3, "bucket")),
                ]);
            }
        }
        if table.is_empty() {
            o.line("Nowhere to back up to yet: add backup storage in the dashboard's Settings.");
        } else {
            o.table(&table);
        }
    })?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> BackupScheduleArgs {
        BackupScheduleArgs {
            service: None,
            hour: None,
            dest: BackupDest::default(),
            daily: None,
            weekly: None,
            monthly: None,
            enable: false,
            disable: false,
        }
    }

    fn current() -> Schedule {
        Schedule {
            enabled: false,
            target_id: "t1".into(),
            target_name: "Disk of main".into(),
            target_kind: "local".into(),
            hour_utc: 3,
            retention: Retention {
                daily_days: 2,
                weekly_days: 0,
                monthly_days: 0,
            },
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(size(None), "-");
        assert_eq!(size(Some(512)), "512 B");
        assert_eq!(size(Some(1536)), "1.5 KB");
        assert_eq!(size(Some(327_155_712)), "312 MB");
    }

    #[test]
    fn new_schedule_needs_a_destination_and_an_hour() {
        assert!(merged(None, &args()).is_err());
        let mut a = args();
        a.dest.local_disk = true;
        assert!(merged(None, &a).is_err());
        a.hour = Some(21);
        let body = merged(None, &a).unwrap();
        assert_eq!(
            body,
            json!({
                "localDisk": true, "hourUtc": 21, "enabled": true,
                "retention": { "dailyDays": 6, "weeklyDays": 27, "monthlyDays": 89 },
            })
        );
    }

    #[test]
    fn flags_change_only_what_they_name() {
        let mut a = args();
        a.hour = Some(20);
        let body = merged(Some(&current()), &a).unwrap();
        // The destination, retention and the off switch are kept.
        assert_eq!(
            body,
            json!({
                "targetId": "t1", "hourUtc": 20, "enabled": false,
                "retention": { "dailyDays": 2, "weeklyDays": 0, "monthlyDays": 0 },
            })
        );
        a.enable = true;
        a.daily = Some(13);
        let body = merged(Some(&current()), &a).unwrap();
        assert_eq!(body["enabled"], true);
        assert_eq!(body["retention"]["dailyDays"], 13);
        assert!(!changes(&args()));
    }
}
