//! Deciding when to notify: event rules on each event (with a cooldown per
//! rule and subject), metric and offline rules every minute (fire once when
//! the condition starts holding, "resolved" once when it stops).
//!
//! Subjects are "instance:<service instance id>" or "node:<node id>". A
//! workspace's servers are the ones it owns plus the shared ones running
//! any of its deployments; a scoped rule only looks at the servers running
//! that project's (or service's) deployments.

use std::collections::HashMap;

use modula_id::Id;
use time::{Duration, OffsetDateTime};

use super::{Alert, Event, Kind, Phase, send};
use crate::jobs::Job;
use crate::state::AppState;

// ------------------------------------------------------------------ pure parts

/// Whether per-minute values held above `threshold` for the whole window:
/// enough minutes have data (a missed sample or two is tolerated) and every
/// one of them is above.
pub fn sustained(minutes: &[f64], window: i32, threshold: f64) -> bool {
    let window = window.max(1) as usize;
    let needed = (window - window / 5).max(1);
    minutes.len() >= needed && minutes.iter().all(|v| *v > threshold)
}

/// A subject's stored state for one rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub firing: bool,
    /// The current incident was sent (so its end is too).
    pub notified: bool,
    pub notified_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    None,
    Fire,
    Resolve,
}

/// Whether the cooldown since the last notification has passed.
pub fn cooled_down(
    notified_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
    cooldown: Duration,
) -> bool {
    notified_at.is_none_or(|t| now - t >= cooldown)
}

/// The next state of a metric or offline rule's subject and what to send.
/// A condition that starts again within the cooldown is tracked but not
/// sent, and then gets no "resolved" either.
pub fn step(
    prev: Option<State>,
    firing: bool,
    now: OffsetDateTime,
    cooldown: Duration,
) -> (State, Action) {
    let prev = prev.unwrap_or(State {
        firing: false,
        notified: false,
        notified_at: None,
    });
    match (prev.firing, firing) {
        (false, true) => {
            let notify = cooled_down(prev.notified_at, now, cooldown);
            let next = State {
                firing: true,
                notified: notify,
                notified_at: if notify { Some(now) } else { prev.notified_at },
            };
            (next, if notify { Action::Fire } else { Action::None })
        }
        (true, false) => {
            let next = State {
                firing: false,
                notified: false,
                notified_at: prev.notified_at,
            };
            (
                next,
                if prev.notified {
                    Action::Resolve
                } else {
                    Action::None
                },
            )
        }
        _ => (prev, Action::None),
    }
}

// ------------------------------------------------------------------ rules

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Rule {
    pub id: Id,
    pub workspace_id: Id,
    pub name: String,
    pub kind: String,
    pub project_id: Option<Id>,
    pub service_id: Option<Id>,
    pub threshold: Option<f32>,
    pub window_minutes: Option<i32>,
    pub cooldown_minutes: i32,
}

macro_rules! rule_columns {
    () => {
        "id, workspace_id, name, kind, project_id, service_id, threshold, window_minutes, cooldown_minutes"
    };
}

/// Where a service instance is, for messages and links.
#[derive(Debug, Clone, sqlx::FromRow)]
struct InstanceCtx {
    workspace_id: Id,
    project_id: Id,
    project: String,
    environment_id: Id,
    environment: String,
    service_id: Id,
    service: String,
}

async fn instance_ctx(db: &sqlx::PgPool, instance: Id) -> sqlx::Result<Option<InstanceCtx>> {
    sqlx::query_as(
        "SELECT p.workspace_id, p.id AS project_id, p.name AS project, e.id AS environment_id,
                e.name AS environment, s.id AS service_id, s.name AS service
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         JOIN environments e ON e.id = i.environment_id
         WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_optional(db)
    .await
}

fn origin(state: &AppState) -> String {
    state.public_origin()
}

fn instance_link(state: &AppState, c: &InstanceCtx, tab: &str, deployment: Option<Id>) -> String {
    let mut link = format!(
        "{}/projects/{}?env={}&service={}&tab={tab}",
        origin(state),
        c.project_id,
        c.environment_id,
        c.service_id
    );
    if let Some(d) = deployment {
        link.push_str(&format!("&deployment={d}"));
    }
    link
}

/// The alert for `rule` about `subject`, with the subject's context filled in.
async fn build(
    state: &AppState,
    rule: &Rule,
    phase: Phase,
    subject: &str,
    title: String,
    reason: String,
    tab: &str,
) -> anyhow::Result<Option<Alert>> {
    let mut alert = Alert {
        phase,
        rule_id: Some(rule.id),
        rule_name: rule.name.clone(),
        kind: Kind::parse(&rule.kind),
        subject: subject.to_owned(),
        title,
        reason,
        project: None,
        environment: None,
        service: None,
        server: None,
        link: origin(state),
        at: OffsetDateTime::now_utc(),
    };
    if let Some(id) = subject.strip_prefix("instance:") {
        let Some(c) = instance_ctx(&state.db, id.parse()?).await? else {
            return Ok(None);
        };
        alert.link = instance_link(state, &c, tab, None);
        alert.project = Some(c.project);
        alert.environment = Some(c.environment);
        alert.service = Some(c.service);
    } else if let Some(id) = subject.strip_prefix("node:") {
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM nodes WHERE id = $1")
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
        let Some(name) = name else { return Ok(None) };
        alert.link = format!("{}/servers", origin(state));
        alert.server = Some(name);
    }
    Ok(Some(alert))
}

// ------------------------------------------------------------------ events

/// Job: the event's matching rules, each notified unless it notified about
/// the same service within its cooldown.
pub async fn event_job(state: AppState, job: Job) -> anyhow::Result<()> {
    let ev: Event = serde_json::from_value(job.payload)?;
    let Some(c) = instance_ctx(&state.db, ev.service_instance_id).await? else {
        return Ok(());
    };
    let rules: Vec<Rule> = sqlx::query_as(concat!(
        "SELECT ",
        rule_columns!(),
        " FROM alert_rules
         WHERE workspace_id = $1 AND enabled AND kind = $2
           AND (project_id IS NULL OR project_id = $3)
           AND (service_id IS NULL OR service_id = $4)
         ORDER BY created_at"
    ))
    .bind(c.workspace_id)
    .bind(ev.kind.as_str())
    .bind(c.project_id)
    .bind(c.service_id)
    .fetch_all(&state.db)
    .await?;
    if rules.is_empty() {
        return Ok(());
    }
    let (title, reason, tab, deployment) = describe_event(&state, &ev, &c).await?;
    let subject = format!("instance:{}", ev.service_instance_id);
    for rule in rules {
        // Claimed atomically: two events at once notify once.
        let due: Option<i32> = sqlx::query_scalar(
            "INSERT INTO alert_states (rule_id, subject, firing, notified, notified_at)
             VALUES ($1, $2, false, true, now())
             ON CONFLICT (rule_id, subject) DO UPDATE SET notified_at = now(), since = now()
             WHERE alert_states.notified_at IS NULL
                OR alert_states.notified_at <= now() - make_interval(mins => $3)
             RETURNING 1",
        )
        .bind(rule.id)
        .bind(&subject)
        .bind(rule.cooldown_minutes)
        .fetch_optional(&state.db)
        .await?;
        if due.is_none() {
            continue;
        }
        let alert = Alert {
            phase: Phase::Fired,
            rule_id: Some(rule.id),
            rule_name: rule.name.clone(),
            kind: Some(ev.kind),
            subject: subject.clone(),
            title: title.clone(),
            reason: reason.clone(),
            project: Some(c.project.clone()),
            environment: Some(c.environment.clone()),
            service: Some(c.service.clone()),
            server: None,
            link: instance_link(&state, &c, tab, deployment),
            at: OffsetDateTime::now_utc(),
        };
        send::notify(&state, rule.id, &alert).await?;
    }
    Ok(())
}

async fn describe_event(
    state: &AppState,
    ev: &Event,
    c: &InstanceCtx,
) -> anyhow::Result<(String, String, &'static str, Option<Id>)> {
    let svc = &c.service;
    let error = |e: Option<String>| e.filter(|e| !e.is_empty()).unwrap_or_default();
    Ok(match ev.kind {
        Kind::BackupFailed => {
            let e: Option<String> = match ev.backup_id {
                Some(b) => sqlx::query_scalar("SELECT error FROM backups WHERE id = $1")
                    .bind(b)
                    .fetch_optional(&state.db)
                    .await?
                    .flatten(),
                None => None,
            };
            (format!("Backup of {svc} failed"), error(e), "backups", None)
        }
        kind => {
            let e: Option<String> = match ev.deployment_id {
                Some(d) => sqlx::query_scalar("SELECT error FROM deployments WHERE id = $1")
                    .bind(d)
                    .fetch_optional(&state.db)
                    .await?
                    .flatten(),
                None => None,
            };
            let title = match kind {
                Kind::BuildFailed => format!("{svc} failed to build"),
                Kind::DeploymentCrashed => format!("{svc} crashed"),
                _ => format!("{svc} failed to deploy"),
            };
            (title, error(e), "deployments", ev.deployment_id)
        }
    })
}

// ------------------------------------------------------------------ metrics

/// One subject's current reading.
struct Reading {
    subject: String,
    firing: bool,
    /// For the message, e.g. "CPU at 93% of its limit".
    reason: String,
}

#[derive(sqlx::FromRow)]
struct UsageRow {
    instance: Id,
    cpu_millis: Option<f64>,
    memory_mb: Option<f64>,
    cpus: Option<f64>,
    cpu: Vec<f64>,
    mem: Vec<f64>,
}

/// Per-minute CPU and memory of the scope's running services over the
/// last `window` complete minutes, as percent of their limits.
async fn usage(state: &AppState, rule: &Rule, window: i32) -> anyhow::Result<Vec<Reading>> {
    let kind = Kind::parse(&rule.kind);
    let threshold = f64::from(rule.threshold.unwrap_or(100.0));
    let rows: Vec<UsageRow> = sqlx::query_as(
        "WITH inst AS (
             SELECT i.id, d.config, n.info
             FROM service_instances i
             JOIN services s ON s.id = i.service_id
             JOIN projects p ON p.id = s.project_id
             JOIN deployments d ON d.service_instance_id = i.id AND d.status = 'active'
             LEFT JOIN nodes n ON n.id = d.node_id
             WHERE p.workspace_id = $1 AND ($2::text IS NULL OR p.id = $2)
               AND ($3::text IS NULL OR s.id = $3)
         )
         SELECT inst.id AS instance, (inst.config->>'cpuMillis')::float8 AS cpu_millis,
                (inst.config->>'memoryMb')::float8 AS memory_mb, (inst.info->>'cpus')::float8 AS cpus,
                array_agg(b.cpu ORDER BY b.minute) AS cpu, array_agg(b.mem ORDER BY b.minute) AS mem
         FROM inst
         JOIN LATERAL (
             SELECT floor(extract(epoch FROM at) / 60) AS minute,
                    avg(cpu_percent)::float8 AS cpu, avg(memory_bytes)::float8 AS mem
             FROM metric_samples
             WHERE service_instance_id = inst.id
               AND at >= date_trunc('minute', now()) - make_interval(mins => $4)
               AND at < date_trunc('minute', now())
             GROUP BY 1
         ) b ON true
         GROUP BY 1, 2, 3, 4",
    )
    .bind(rule.workspace_id)
    .bind(rule.project_id)
    .bind(rule.service_id)
    .bind(window)
    .fetch_all(&state.db)
    .await?;
    let mut out = Vec::new();
    for r in rows {
        let (values, what): (Vec<f64>, &str) = match kind {
            Some(Kind::Cpu) => {
                // Percent of the limit, else of the server's CPUs.
                let cores = r
                    .cpu_millis
                    .map(|m| m / 1000.0)
                    .or(r.cpus)
                    .filter(|c| *c > 0.0)
                    .unwrap_or(1.0);
                (r.cpu.iter().map(|c| c / cores).collect(), "CPU")
            }
            Some(Kind::Memory) => {
                let Some(limit) = r.memory_mb.filter(|m| *m > 0.0) else {
                    continue;
                };
                let limit = limit * 1024.0 * 1024.0;
                (r.mem.iter().map(|m| m / limit * 100.0).collect(), "Memory")
            }
            _ => continue,
        };
        let latest = values.last().copied().unwrap_or(0.0);
        out.push(Reading {
            subject: format!("instance:{}", r.instance),
            firing: sustained(&values, window, threshold),
            reason: format!(
                "{what} at {latest:.0}% of its limit (above {threshold:.0}% for {window} min)"
            ),
        });
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct NodeRow {
    id: Id,
    offline_minutes: Option<f64>,
    disk_percent: Option<f64>,
    disk_path: Option<String>,
}

/// The scope's servers, with how long each has been silent and how full its
/// fullest disk was at the last report (within the last half hour).
async fn nodes(state: &AppState, rule: &Rule) -> anyhow::Result<Vec<NodeRow>> {
    Ok(sqlx::query_as(
        "SELECT n.id,
                extract(epoch FROM now() - coalesce(n.last_seen_at, n.created_at))::float8 / 60
                    AS offline_minutes,
                disk.percent AS disk_percent, disk.path AS disk_path
         FROM nodes n
         LEFT JOIN LATERAL (
             SELECT ((1 - free_bytes::float8 / total_bytes) * 100) AS percent, path
             FROM disk_samples
             WHERE node_id = n.id AND total_bytes > 0
               AND at = (SELECT max(at) FROM disk_samples WHERE node_id = n.id
                         AND at > now() - interval '30 minutes')
             ORDER BY 1 DESC LIMIT 1
         ) disk ON true
         WHERE n.status <> 'revoked'
           AND ((n.workspace_id = $1 AND $2::text IS NULL)
                OR EXISTS (
                    SELECT 1 FROM deployment_replicas r
                    JOIN deployments d ON d.id = r.deployment_id
                    JOIN service_instances i ON i.id = d.service_instance_id
                    JOIN services s ON s.id = i.service_id
                    JOIN projects p ON p.id = s.project_id
                    WHERE r.node_id = n.id AND d.status IN ('deploying', 'active', 'sleeping', 'crashed')
                      AND p.workspace_id = $1 AND ($2::text IS NULL OR p.id = $2)
                      AND ($3::text IS NULL OR s.id = $3)))",
    )
    .bind(rule.workspace_id)
    .bind(rule.project_id)
    .bind(rule.service_id)
    .fetch_all(&state.db)
    .await?)
}

async fn readings(state: &AppState, rule: &Rule) -> anyhow::Result<Vec<Reading>> {
    let threshold = f64::from(rule.threshold.unwrap_or(100.0));
    let window = rule.window_minutes.unwrap_or(5);
    Ok(match Kind::parse(&rule.kind) {
        Some(Kind::Cpu | Kind::Memory) => usage(state, rule, window).await?,
        Some(Kind::NodeOffline) => nodes(state, rule)
            .await?
            .into_iter()
            .map(|n| {
                let m = n.offline_minutes.unwrap_or(0.0);
                Reading {
                    subject: format!("node:{}", n.id),
                    firing: m >= f64::from(window),
                    reason: format!("No contact for {m:.0} min"),
                }
            })
            .collect(),
        Some(Kind::Disk) => nodes(state, rule)
            .await?
            .into_iter()
            .filter_map(|n| {
                let p = n.disk_percent?;
                Some(Reading {
                    subject: format!("node:{}", n.id),
                    firing: p > threshold,
                    reason: format!(
                        "{} is {p:.0}% full (above {threshold:.0}%)",
                        n.disk_path.unwrap_or_else(|| "The disk".into())
                    ),
                })
            })
            .collect(),
        _ => Vec::new(),
    })
}

fn title(kind: Option<Kind>, phase: Phase) -> &'static str {
    match (kind, phase) {
        (Some(Kind::Cpu), Phase::Resolved) => "CPU back to normal",
        (Some(Kind::Cpu), _) => "High CPU",
        (Some(Kind::Memory), Phase::Resolved) => "Memory back to normal",
        (Some(Kind::Memory), _) => "High memory",
        (Some(Kind::Disk), Phase::Resolved) => "Disk usage back to normal",
        (Some(Kind::Disk), _) => "Disk almost full",
        (Some(Kind::NodeOffline), Phase::Resolved) => "Server back online",
        (Some(Kind::NodeOffline), _) => "Server offline",
        _ => "Alert",
    }
}

/// One rule's evaluation: compares each subject's reading with its stored
/// state, stores the new one and sends what changed. Subjects that left the
/// scope (or stopped reporting) count as cleared.
pub async fn evaluate_rule(state: &AppState, rule: &Rule) -> anyhow::Result<()> {
    let now = OffsetDateTime::now_utc();
    let cooldown = Duration::minutes(i64::from(rule.cooldown_minutes));
    let mut current: HashMap<String, Reading> = readings(state, rule)
        .await?
        .into_iter()
        .map(|r| (r.subject.clone(), r))
        .collect();
    #[derive(sqlx::FromRow)]
    struct Stored {
        subject: String,
        firing: bool,
        notified: bool,
        notified_at: Option<OffsetDateTime>,
    }
    let stored: Vec<Stored> = sqlx::query_as(
        "SELECT subject, firing, notified, notified_at FROM alert_states WHERE rule_id = $1",
    )
    .bind(rule.id)
    .fetch_all(&state.db)
    .await?;
    let mut prev: HashMap<String, State> = stored
        .into_iter()
        .map(|s| {
            (
                s.subject,
                State {
                    firing: s.firing,
                    notified: s.notified,
                    notified_at: s.notified_at,
                },
            )
        })
        .collect();
    for (subject, p) in &prev {
        if p.firing && !current.contains_key(subject) {
            current.insert(
                subject.clone(),
                Reading {
                    subject: subject.clone(),
                    firing: false,
                    reason: String::new(),
                },
            );
        }
    }
    let kind = Kind::parse(&rule.kind);
    let tab = if matches!(kind, Some(Kind::Cpu | Kind::Memory)) {
        "metrics"
    } else {
        "deployments"
    };
    for (subject, reading) in current {
        let before = prev.remove(&subject);
        let (next, action) = step(before, reading.firing, now, cooldown);
        if before != Some(next) {
            sqlx::query(
                "INSERT INTO alert_states (rule_id, subject, firing, notified, notified_at, since)
                 VALUES ($1, $2, $3, $4, $5, now())
                 ON CONFLICT (rule_id, subject) DO UPDATE SET firing = $3, notified = $4,
                     notified_at = $5,
                     since = CASE WHEN alert_states.firing <> $3 THEN now() ELSE alert_states.since END",
            )
            .bind(rule.id)
            .bind(&subject)
            .bind(next.firing)
            .bind(next.notified)
            .bind(next.notified_at)
            .execute(&state.db)
            .await?;
        }
        let phase = match action {
            Action::None => continue,
            Action::Fire => Phase::Fired,
            Action::Resolve => Phase::Resolved,
        };
        let title = title(kind, phase).to_owned();
        if let Some(mut alert) =
            build(state, rule, phase, &subject, title, reading.reason, tab).await?
        {
            if let Some(s) = alert.service.as_deref().or(alert.server.as_deref()) {
                alert.title = format!("{}: {s}", alert.title);
            }
            send::notify(state, rule.id, &alert).await?;
        }
    }
    Ok(())
}

/// Job: every enabled metric and offline rule (queued every minute).
pub async fn evaluate_job(state: AppState, _job: Job) -> anyhow::Result<()> {
    let rules: Vec<Rule> = sqlx::query_as(concat!(
        "SELECT ",
        rule_columns!(),
        " FROM alert_rules
         WHERE enabled AND kind IN ('cpu', 'memory', 'disk', 'node_offline')
         ORDER BY created_at"
    ))
    .fetch_all(&state.db)
    .await?;
    for rule in rules {
        if let Err(e) = evaluate_rule(&state, &rule).await {
            tracing::warn!(rule = %rule.id, error = format!("{e:#}"), "alert rule evaluation failed");
        }
    }
    Ok(())
}

/// Queues [`evaluate_job`] every minute until `stop` (one queued at a time
/// across server replicas).
pub async fn schedule_loop(state: AppState, stop: tokio_util::sync::CancellationToken) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tick.tick() => {
                let job = crate::jobs::NewJob {
                    kind: super::EVALUATE_JOB,
                    dedupe_key: Some(super::EVALUATE_JOB),
                    max_attempts: Some(1),
                    ..Default::default()
                };
                match crate::jobs::enqueue(&state.db, job).await {
                    Ok(_) => state.jobs_notify.notify_one(),
                    Err(e) => tracing::warn!(error = %e, "could not schedule alert evaluation"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sustained_needs_the_whole_window_above() {
        assert!(sustained(&[91.0, 95.0, 92.0, 99.0, 93.0], 5, 90.0));
        // One minute below: not sustained.
        assert!(!sustained(&[91.0, 95.0, 80.0, 99.0, 93.0], 5, 90.0));
        // A missing minute is tolerated, two aren't.
        assert!(sustained(&[91.0, 95.0, 99.0, 93.0], 5, 90.0));
        assert!(!sustained(&[91.0, 95.0, 99.0], 5, 90.0));
        // Equal is not above.
        assert!(!sustained(&[90.0], 1, 90.0));
        assert!(sustained(&[90.5], 1, 90.0));
        assert!(!sustained(&[], 1, 90.0));
    }

    #[test]
    fn fires_once_then_resolves() {
        let t0 = time::macros::datetime!(2026-10-24 12:00 UTC);
        let cd = Duration::minutes(30);
        let (s, a) = step(None, true, t0, cd);
        assert_eq!(a, Action::Fire);
        assert!(s.firing && s.notified);
        // Still firing: nothing new.
        let (s, a) = step(Some(s), true, t0 + Duration::minutes(1), cd);
        assert_eq!(a, Action::None);
        // Clears: resolved.
        let (s, a) = step(Some(s), false, t0 + Duration::minutes(2), cd);
        assert_eq!(a, Action::Resolve);
        assert!(!s.firing);
        // Fires again within the cooldown: tracked, not sent, and its end isn't either.
        let (s, a) = step(Some(s), true, t0 + Duration::minutes(10), cd);
        assert_eq!(a, Action::None);
        assert!(s.firing && !s.notified);
        let (s, a) = step(Some(s), false, t0 + Duration::minutes(11), cd);
        assert_eq!(a, Action::None);
        // After the cooldown it is sent again.
        let (_, a) = step(Some(s), true, t0 + Duration::minutes(31), cd);
        assert_eq!(a, Action::Fire);
        // Never firing: nothing.
        assert_eq!(step(None, false, t0, cd).1, Action::None);
    }

    #[test]
    fn cooldown() {
        let t0 = time::macros::datetime!(2026-10-24 12:00 UTC);
        let cd = Duration::minutes(30);
        assert!(cooled_down(None, t0, cd));
        assert!(!cooled_down(Some(t0), t0 + Duration::minutes(29), cd));
        assert!(cooled_down(Some(t0), t0 + Duration::minutes(30), cd));
        assert!(cooled_down(Some(t0), t0, Duration::ZERO));
    }
}
