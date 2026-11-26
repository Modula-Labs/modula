//! The weekly traffic report: per workspace, opt-in, sent every Monday at
//! 09:00 in the workspace's chosen timezone to the channels it picked.
//!
//! It reads the edge's per-minute rollups (`http_rollups`, see
//! [`crate::http_logs`]): requests, 5xx error rate and latency percentiles
//! for the 7 days before the scheduled time against the 7 days before
//! those, and the busiest services. The rollups are kept
//! [`crate::http_logs::ROLLUP_RETENTION_DAYS`] days so the week before is
//! still there.
//!
//! The minute alert evaluation ([`super::eval::evaluate_job`]) sends the
//! reports that are due; a server that was down sends a missed report once
//! and moves on to the next Monday.

use std::collections::BTreeMap;

use jiff::civil::{Time, Weekday};
use jiff::tz::TimeZone;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use super::format::{Mail, Page, html_escape, origin_of, page, paragraph, settings_link};
use super::send::{self, Channel};
use super::{Alert, Phase};
use crate::http_logs::{BUCKETS, percentile};
use crate::state::AppState;

/// Monday, 09:00 local time.
pub const WEEKDAY: Weekday = Weekday::Monday;
pub const HOUR: i8 = 9;
pub const DEFAULT_TIMEZONE: &str = "UTC";
/// Services listed in the report.
const TOP: usize = 5;

/// An IANA timezone name the bundled database knows (`Asia/Kolkata`).
pub fn timezone(name: &str) -> Option<TimeZone> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 {
        return None;
    }
    TimeZone::get(n).ok()
}

/// The first Monday 09:00 in `tz` strictly after `after`, as UTC. A 09:00
/// that a DST change skips becomes the instant after the gap.
pub fn next_run(tz: &TimeZone, after: OffsetDateTime) -> OffsetDateTime {
    let Ok(ts) = jiff::Timestamp::from_second(after.unix_timestamp()) else {
        return after + Duration::days(7);
    };
    let today = ts.to_zoned(tz.clone()).date();
    for d in 0..=7 {
        let Ok(date) = today.checked_add(jiff::Span::new().days(d)) else {
            break;
        };
        if date.weekday() != WEEKDAY {
            continue;
        }
        let Ok(at) = date
            .to_datetime(Time::constant(HOUR, 0, 0, 0))
            .to_zoned(tz.clone())
        else {
            continue;
        };
        let secs = at.timestamp().as_second();
        if secs > after.unix_timestamp()
            && let Ok(t) = OffsetDateTime::from_unix_timestamp(secs)
        {
            return t;
        }
    }
    after + Duration::days(7)
}

// ------------------------------------------------------------------ data

/// One week of one service (or of the workspace).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stats {
    pub requests: i64,
    pub status_4xx: i64,
    pub status_5xx: i64,
    /// Latency histogram, [`BUCKETS`] long.
    pub hist: Vec<i64>,
}

impl Stats {
    fn add(&mut self, o: &Stats) {
        self.requests += o.requests;
        self.status_4xx += o.status_4xx;
        self.status_5xx += o.status_5xx;
        if self.hist.len() < o.hist.len() {
            self.hist.resize(o.hist.len(), 0);
        }
        for (a, b) in self.hist.iter_mut().zip(&o.hist) {
            *a += b;
        }
    }

    /// Share of requests answered 5xx, in percent.
    pub fn error_rate(&self) -> Option<f64> {
        (self.requests > 0).then(|| self.status_5xx as f64 / self.requests as f64 * 100.0)
    }

    pub fn p50(&self) -> Option<f64> {
        percentile(&self.hist, 0.5)
    }

    pub fn p95(&self) -> Option<f64> {
        percentile(&self.hist, 0.95)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceWeek {
    pub project_id: Id,
    pub environment_id: Id,
    pub project: String,
    pub environment: String,
    pub service: String,
    pub current: Stats,
    pub previous: Stats,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub workspace_id: Option<Id>,
    pub workspace: String,
    /// The 7 days reported on: `[from, to)`.
    pub from: OffsetDateTime,
    pub to: OffsetDateTime,
    pub current: Stats,
    pub previous: Stats,
    /// The busiest services this week, most requests first.
    pub top: Vec<ServiceWeek>,
    /// Services that served requests this week.
    pub services: usize,
    pub link: String,
}

#[derive(sqlx::FromRow)]
struct Row {
    instance: Id,
    project_id: Id,
    environment_id: Id,
    project: String,
    environment: String,
    service: String,
    current: bool,
    requests: i64,
    status_4xx: i64,
    status_5xx: i64,
    hist: Vec<i64>,
}

/// Per service instance, this week's and last week's traffic.
pub async fn collect(
    db: &sqlx::PgPool,
    workspace: Id,
    to: OffsetDateTime,
) -> sqlx::Result<(Stats, Stats, Vec<ServiceWeek>)> {
    let from = to - Duration::days(7);
    let before = from - Duration::days(7);
    let rows: Vec<Row> = sqlx::query_as(
        "WITH r AS (
             SELECT r.service_instance_id AS instance, r.minute >= $3 AS current,
                    r.requests, r.status_4xx, r.status_5xx, r.latency_hist
             FROM http_rollups r
             JOIN service_instances i ON i.id = r.service_instance_id
             JOIN services s ON s.id = i.service_id
             JOIN projects p ON p.id = s.project_id
             WHERE p.workspace_id = $1 AND r.minute >= $2 AND r.minute < $4
         ),
         counts AS (
             SELECT instance, current, sum(requests)::bigint AS requests,
                    sum(status_4xx)::bigint AS status_4xx, sum(status_5xx)::bigint AS status_5xx
             FROM r GROUP BY 1, 2
         ),
         hist AS (
             SELECT instance, current, array_agg(n ORDER BY idx) AS hist
             FROM (SELECT instance, current, h.idx, sum(h.n)::bigint AS n
                   FROM r, unnest(r.latency_hist) WITH ORDINALITY AS h(n, idx)
                   GROUP BY 1, 2, 3) x
             GROUP BY 1, 2
         )
         SELECT c.instance, p.id AS project_id, e.id AS environment_id, p.name AS project, e.name AS environment, s.name AS service,
                c.current, c.requests, c.status_4xx, c.status_5xx,
                coalesce(h.hist, '{}') AS hist
         FROM counts c
         LEFT JOIN hist h ON h.instance = c.instance AND h.current = c.current
         JOIN service_instances i ON i.id = c.instance
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         JOIN environments e ON e.id = i.environment_id",
    )
    .bind(workspace)
    .bind(before)
    .bind(from)
    .bind(to)
    .fetch_all(db)
    .await?;
    Ok(summarize(rows))
}

fn summarize(rows: Vec<Row>) -> (Stats, Stats, Vec<ServiceWeek>) {
    let mut by: BTreeMap<Id, ServiceWeek> = BTreeMap::new();
    for r in rows {
        let s = by.entry(r.instance).or_insert_with(|| ServiceWeek {
            project_id: r.project_id,
            environment_id: r.environment_id,
            project: r.project.clone(),
            environment: r.environment.clone(),
            service: r.service.clone(),
            current: Stats::default(),
            previous: Stats::default(),
        });
        let stats = Stats {
            requests: r.requests,
            status_4xx: r.status_4xx,
            status_5xx: r.status_5xx,
            hist: r.hist,
        };
        if r.current {
            s.current.add(&stats);
        } else {
            s.previous.add(&stats);
        }
    }
    let mut current = Stats {
        hist: vec![0; BUCKETS],
        ..Default::default()
    };
    let mut previous = current.clone();
    for s in by.values() {
        current.add(&s.current);
        previous.add(&s.previous);
    }
    let mut services: Vec<ServiceWeek> = by.into_values().collect();
    services.sort_by(|a, b| {
        b.current
            .requests
            .cmp(&a.current.requests)
            .then_with(|| a.service.cmp(&b.service))
    });
    (current, previous, services)
}

// ------------------------------------------------------------------ format

/// "12,345".
pub fn thousands(n: i64) -> String {
    let s = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// "+12%", "-3%", "new" (nothing the week before), "±0%".
pub fn change(now: i64, before: i64) -> String {
    if before <= 0 {
        return if now > 0 { "new".into() } else { "±0%".into() };
    }
    let pct = (now - before) as f64 / before as f64 * 100.0;
    match pct.round() as i64 {
        0 => "±0%".into(),
        p if p > 0 => format!("+{p}%"),
        p => format!("{p}%"),
    }
}

fn ms(v: Option<f64>) -> String {
    match v {
        Some(v) if v >= 1000.0 => format!("{:.2} s", v / 1000.0),
        Some(v) => format!("{v:.0} ms"),
        None => "–".into(),
    }
}

fn rate(v: Option<f64>) -> String {
    match v {
        Some(v) => format!("{v:.2}%"),
        None => "–".into(),
    }
}

fn day(t: OffsetDateTime) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{} {}",
        t.day(),
        MONTHS[usize::from(u8::from(t.month())) - 1]
    )
}

impl Report {
    /// "17 Nov – 24 Nov": the last day shown is the one before `to`.
    pub fn period(&self) -> String {
        format!(
            "{} – {}",
            day(self.from),
            day(self.to - Duration::seconds(1))
        )
    }

    pub fn title(&self) -> String {
        format!("Weekly traffic for {}: {}", self.workspace, self.period())
    }

    fn summary_lines(&self) -> Vec<(&'static str, String)> {
        let (c, p) = (&self.current, &self.previous);
        vec![
            (
                "Requests",
                format!(
                    "{} ({} vs the week before: {})",
                    thousands(c.requests),
                    change(c.requests, p.requests),
                    thousands(p.requests)
                ),
            ),
            (
                "Error rate (5xx)",
                format!(
                    "{} (was {}); {} server errors, {} client errors (4xx)",
                    rate(c.error_rate()),
                    rate(p.error_rate()),
                    thousands(c.status_5xx),
                    thousands(c.status_4xx)
                ),
            ),
            (
                "Latency p50 / p95",
                format!(
                    "{} / {} (was {} / {})",
                    ms(c.p50()),
                    ms(c.p95()),
                    ms(p.p50()),
                    ms(p.p95())
                ),
            ),
        ]
    }

    fn top(&self) -> &[ServiceWeek] {
        &self.top[..self.top.len().min(TOP)]
    }

    /// Plain text, also the body Slack, Discord and webhooks get.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for (k, v) in self.summary_lines() {
            out.push_str(&format!("{k}: {v}\n"));
        }
        if self.current.requests == 0 {
            out.push_str("\nNo requests reached your services through the edge this week.\n");
            return out;
        }
        out.push_str(&format!(
            "\nBusiest services ({} served traffic):\n",
            self.services
        ));
        for s in self.top() {
            out.push_str(&format!(
                "- {} / {} ({}): {} requests ({}), errors {}, p95 {}\n",
                s.project,
                s.service,
                s.environment,
                thousands(s.current.requests),
                change(s.current.requests, s.previous.requests),
                rate(s.current.error_rate()),
                ms(s.current.p95())
            ));
        }
        out
    }

    /// A service's traffic in the dashboard (the project's metrics page).
    pub fn service_link(&self, s: &ServiceWeek) -> String {
        format!(
            "{}/projects/{}/metrics?env={}",
            origin_of(&self.link),
            s.project_id,
            s.environment_id
        )
    }

    pub fn mail(&self) -> Mail {
        let title = self.title();
        let origin = origin_of(&self.link);
        let settings = settings_link(&origin, self.workspace_id);
        let projects = match self.workspace_id {
            Some(w) => format!("{origin}/projects?workspace={w}"),
            None => format!("{origin}/projects"),
        };
        let mut text = format!("{title}\n\n{}", self.text());
        text.push_str(&format!(
            "\nOpen in Modula: {projects}\nReport settings: {settings}\n"
        ));

        let (c, p) = (&self.current, &self.previous);
        let tile = |label: &str, value: String, note: String| {
            format!(
                "<td valign=\"top\" style=\"width:33%;padding:16px;background:#f6f7f8;border-radius:10px\">\
<div style=\"font-size:12px;font-weight:600;letter-spacing:0.3px;text-transform:uppercase;color:#6b6f76\">{}</div>\
<div style=\"margin-top:6px;font-size:24px;font-weight:700;color:#1c1d1f\">{}</div>\
<div style=\"margin-top:2px;font-size:13px;color:#6b6f76\">{}</div></td>",
                html_escape(label),
                html_escape(&value),
                html_escape(&note)
            )
        };
        let gap = "<td style=\"width:10px\"></td>";
        let tiles = format!(
            "<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"margin:24px 0 8px\"><tr>{}{gap}{}{gap}{}</tr></table>",
            tile(
                "Requests",
                thousands(c.requests),
                format!("{} vs last week", change(c.requests, p.requests))
            ),
            tile(
                "Error rate",
                rate(c.error_rate()),
                format!("was {}", rate(p.error_rate()))
            ),
            tile("p95 latency", ms(c.p95()), format!("was {}", ms(p.p95()))),
        );
        let detail = paragraph(&format!(
            "{} server errors (5xx) and {} client errors (4xx). Median latency {}.",
            html_escape(&thousands(c.status_5xx)),
            html_escape(&thousands(c.status_4xx)),
            html_escape(&ms(c.p50()))
        ));
        let services = if self.current.requests == 0 {
            paragraph("No requests reached your services through Modula's edge this week.")
        } else {
            let cell = "padding:12px 10px 12px 0;border-bottom:1px solid #eceef0;font-size:14px";
            let rows: String = self
                .top()
                .iter()
                .map(|s| {
                    format!(
                        "<tr><td style=\"{cell}\"><a href=\"{}\" style=\"color:#0f766e;text-decoration:none;font-weight:600\">{}</a>\
<div style=\"font-size:12px;color:#6b6f76\">{} · {}</div></td>\
<td style=\"{cell};text-align:right;color:#1c1d1f\">{}<div style=\"font-size:12px;color:#6b6f76\">{}</div></td>\
<td style=\"{cell};text-align:right;color:#1c1d1f\">{}</td><td style=\"{cell};text-align:right;padding-right:0;color:#1c1d1f\">{}</td></tr>",
                        html_escape(&self.service_link(s)),
                        html_escape(&s.service),
                        html_escape(&s.project),
                        html_escape(&s.environment),
                        thousands(s.current.requests),
                        change(s.current.requests, s.previous.requests),
                        rate(s.current.error_rate()),
                        ms(s.current.p95()),
                    )
                })
                .collect();
            let th = "padding:0 10px 8px 0;text-align:right;font-size:12px;font-weight:600;letter-spacing:0.3px;text-transform:uppercase;color:#6b6f76;border-bottom:1px solid #eceef0";
            format!(
                "<h2 style=\"margin:32px 0 12px;font-size:16px;font-weight:600;color:#1c1d1f\">Busiest services</h2>\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"border-collapse:collapse\">\
<tr><th style=\"{th};text-align:left\">Service</th><th style=\"{th}\">Requests</th><th style=\"{th}\">Errors</th><th style=\"{th};padding-right:0\">p95</th></tr>{rows}</table>"
            )
        };
        let intro = paragraph(&format!(
            "Here's how traffic through Modula's edge looked in {} for {}, compared with the week before.",
            super::format::bold_html(&self.workspace),
            html_escape(&self.period())
        ));
        let body = format!("{intro}{tiles}{detail}{services}");
        let primary_url = self
            .top()
            .first()
            .map(|s| self.service_link(s))
            .unwrap_or_else(|| projects.clone());
        let primary_label = if self.top().is_empty() {
            "Open Modula"
        } else {
            "View traffic"
        };
        let footer = format!(
            "Sent every Monday at 09:00 to this workspace's report channels. \
<a href=\"{}\" style=\"color:#6b6f76\">Change or turn off the weekly report</a>",
            html_escape(&settings)
        );
        let preheader = format!(
            "{} requests ({}), {} errors, p95 {}",
            thousands(c.requests),
            change(c.requests, p.requests),
            rate(c.error_rate()),
            ms(c.p95())
        );
        let html = page(&Page {
            preheader: &preheader,
            badge: ("Weekly report", "#0F766E"),
            title: &title,
            body,
            primary: Some((primary_label, &primary_url)),
            secondary: vec![
                ("All projects", projects.clone()),
                ("Report settings", settings.clone()),
            ],
            steps: vec![],
            footer,
            origin: &origin,
        });
        Mail {
            subject: format!("[Modula] {title}"),
            text,
            html,
        }
    }

    /// The report as an alert, for Slack, Discord and webhooks.
    pub fn alert(&self, workspace: Id) -> Alert {
        Alert {
            phase: Phase::Report,
            rule_id: None,
            rule_name: "Weekly traffic report".into(),
            kind: None,
            subject: format!("workspace:{workspace}"),
            title: self.title(),
            reason: self.text(),
            project: None,
            environment: None,
            service: None,
            server: None,
            link: self.link.clone(),
            at: self.to,
        }
    }
}

// ------------------------------------------------------------------ settings

/// A workspace's report settings (off without a row).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub enabled: bool,
    pub timezone: String,
    pub channel_ids: Vec<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub next_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_sent_at: Option<OffsetDateTime>,
}

pub async fn settings(db: &sqlx::PgPool, workspace: Id) -> sqlx::Result<Settings> {
    let row: Option<Settings> = sqlx::query_as(
        "SELECT t.enabled, t.timezone, t.next_at, t.last_sent_at,
                coalesce((SELECT array_agg(c.channel_id::text ORDER BY c.channel_id)
                          FROM traffic_report_channels c WHERE c.workspace_id = t.workspace_id),
                         '{}') AS channel_ids
         FROM traffic_reports t WHERE t.workspace_id = $1",
    )
    .bind(workspace)
    .fetch_optional(db)
    .await?;
    Ok(row.unwrap_or(Settings {
        enabled: false,
        timezone: DEFAULT_TIMEZONE.into(),
        channel_ids: Vec::new(),
        next_at: None,
        last_sent_at: None,
    }))
}

// ------------------------------------------------------------------ sending

/// Builds the workspace's report for the week ending at `to`.
pub async fn build(state: &AppState, workspace: Id, to: OffsetDateTime) -> anyhow::Result<Report> {
    let name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id = $1")
        .bind(workspace)
        .fetch_one(&state.db)
        .await?;
    let (current, previous, all) = collect(&state.db, workspace, to).await?;
    let services = all.iter().filter(|s| s.current.requests > 0).count();
    Ok(Report {
        workspace_id: Some(workspace),
        workspace: name,
        from: to - Duration::days(7),
        to,
        current,
        previous,
        top: all.into_iter().filter(|s| s.current.requests > 0).collect(),
        services,
        link: state.public_origin().to_string(),
    })
}

/// Sends `report` to `channels`; a failing channel doesn't stop the others.
pub async fn deliver(state: &AppState, workspace: Id, report: &Report, channels: &[Channel]) {
    let alert = report.alert(workspace);
    let mail = report.mail();
    for c in channels {
        match send::deliver_with(state, c, &alert, Some(&mail)).await {
            Ok(d) if d.error.is_some() => {
                tracing::warn!(channel = %c.id, error = ?d.error, "traffic report delivery failed")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(channel = %c.id, error = ?e, "traffic report delivery failed"),
        }
    }
}

pub async fn channels(state: &AppState, workspace: Id) -> sqlx::Result<Vec<Channel>> {
    send::report_channels(state, workspace).await
}

/// Sends every report that is due (from the minute evaluation). Each is
/// claimed by moving its `next_at` first, so replicas send it once.
pub async fn send_due(state: &AppState) -> anyhow::Result<()> {
    loop {
        let mut tx = state.db.begin().await?;
        let due: Option<(Id, String, OffsetDateTime)> = sqlx::query_as(
            "SELECT workspace_id, timezone, next_at FROM traffic_reports
             WHERE enabled AND next_at <= now()
             ORDER BY next_at LIMIT 1 FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;
        let Some((workspace, tz, at)) = due else {
            return Ok(());
        };
        let tz = timezone(&tz).unwrap_or(TimeZone::UTC);
        let now = OffsetDateTime::now_utc();
        sqlx::query(
            "UPDATE traffic_reports SET next_at = $2, last_sent_at = now() WHERE workspace_id = $1",
        )
        .bind(workspace)
        .bind(next_run(&tz, now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        // A report missed by more than a day covers the week before now.
        let to = if now - at > Duration::days(1) {
            now
        } else {
            at
        };
        match build(state, workspace, to).await {
            Ok(report) => {
                let channels = channels(state, workspace).await?;
                deliver(state, workspace, &report, &channels).await;
            }
            Err(e) => {
                tracing::warn!(%workspace, error = format!("{e:#}"), "could not build traffic report")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn next_monday_nine_in_kolkata_is_0330_utc() {
        let tz = timezone("Asia/Kolkata").unwrap();
        // Wednesday 25 Nov 2026 → Monday 30 Nov 09:00 IST.
        assert_eq!(
            next_run(&tz, datetime!(2026-11-25 10:00 UTC)),
            datetime!(2026-11-30 03:30 UTC)
        );
        // Monday just before 09:00 IST: today.
        assert_eq!(
            next_run(&tz, datetime!(2026-11-30 03:29 UTC)),
            datetime!(2026-11-30 03:30 UTC)
        );
        // At 09:00 itself: next week.
        assert_eq!(
            next_run(&tz, datetime!(2026-11-30 03:30 UTC)),
            datetime!(2026-12-07 03:30 UTC)
        );
        // Sunday 23:00 UTC is already Monday 04:30 IST: that Monday at 09:00.
        assert_eq!(
            next_run(&tz, datetime!(2026-11-29 23:00 UTC)),
            datetime!(2026-11-30 03:30 UTC)
        );
    }

    #[test]
    fn next_run_follows_daylight_saving() {
        let tz = timezone("Europe/London").unwrap();
        // Summer time: 08:00 UTC; winter: 09:00 UTC (DST ended 25 Oct 2026).
        assert_eq!(
            next_run(&tz, datetime!(2026-10-20 00:00 UTC)),
            datetime!(2026-10-26 09:00 UTC)
        );
        assert_eq!(
            next_run(&tz, datetime!(2026-10-12 00:00 UTC)),
            datetime!(2026-10-12 08:00 UTC)
        );
        let utc = timezone("UTC").unwrap();
        assert_eq!(
            next_run(&utc, datetime!(2026-11-25 10:00 UTC)),
            datetime!(2026-11-30 09:00 UTC)
        );
        assert!(timezone("Mars/Olympus").is_none());
        assert!(timezone("").is_none());
    }

    fn hist(at_bucket: &[(usize, i64)]) -> Vec<i64> {
        let mut h = vec![0; BUCKETS];
        for &(i, n) in at_bucket {
            h[i] = n;
        }
        h
    }

    fn row(svc: &str, current: bool, requests: i64, s5xx: i64, h: Vec<i64>) -> Row {
        Row {
            instance: format!("{svc:a>16}").parse().unwrap(),
            project_id: "pppppppppppppppp".parse().unwrap(),
            environment_id: "eeeeeeeeeeeeeeee".parse().unwrap(),
            project: "shop".into(),
            environment: "production".into(),
            service: svc.into(),
            current,
            requests,
            status_4xx: 1,
            status_5xx: s5xx,
            hist: h,
        }
    }

    #[test]
    fn weeks_add_up_and_services_rank_by_requests() {
        let (c, p, top) = summarize(vec![
            row("api", true, 900, 9, hist(&[(4, 900)])),
            row("api", false, 600, 0, hist(&[(4, 600)])),
            row("web", true, 100, 0, hist(&[(2, 100)])),
            row("old", false, 50, 5, hist(&[(8, 50)])),
        ]);
        assert_eq!(c.requests, 1000);
        assert_eq!(p.requests, 650);
        assert_eq!(c.status_5xx, 9);
        assert_eq!(c.status_4xx, 2);
        assert!((c.error_rate().unwrap() - 0.9).abs() < 1e-9);
        assert_eq!(c.hist.iter().sum::<i64>(), 1000);
        let names: Vec<&str> = top.iter().map(|s| s.service.as_str()).collect();
        assert_eq!(names, ["api", "web", "old"]);
        assert_eq!(top[0].previous.requests, 600);
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(change(120, 100), "+20%");
        assert_eq!(change(90, 100), "-10%");
        assert_eq!(change(100, 100), "±0%");
        assert_eq!(change(5, 0), "new");
        assert_eq!(change(0, 0), "±0%");
        assert_eq!(ms(Some(1500.0)), "1.50 s");
        assert_eq!(ms(Some(23.4)), "23 ms");
        assert_eq!(ms(None), "–");
    }

    fn report(requests: i64) -> Report {
        let (current, previous, top) = summarize(vec![
            row("api", true, requests, 2, hist(&[(4, requests)])),
            row("api", false, 50, 0, hist(&[(5, 50)])),
        ]);
        Report {
            workspace_id: Some("wwwwwwwwwwwwwwww".parse().unwrap()),
            workspace: "Personal <&>".into(),
            from: datetime!(2026-11-23 03:30 UTC),
            to: datetime!(2026-11-30 03:30 UTC),
            current,
            previous,
            top: top.into_iter().filter(|s| s.current.requests > 0).collect(),
            services: 1,
            link: "https://cloud.example.com".into(),
        }
    }

    #[test]
    fn report_text_and_mail() {
        let r = report(100);
        assert_eq!(r.period(), "23 Nov – 30 Nov");
        assert_eq!(
            r.title(),
            "Weekly traffic for Personal <&>: 23 Nov – 30 Nov"
        );
        let t = r.text();
        assert!(
            t.starts_with("Requests: 100 (+100% vs the week before: 50)\n"),
            "{t}"
        );
        assert!(t.contains("Error rate (5xx): 2.00% (was 0.00%)"), "{t}");
        assert!(t.contains("Latency p50 / p95: "), "{t}");
        assert!(
            t.contains("- shop / api (production): 100 requests (+100%), errors 2.00%"),
            "{t}"
        );
        let m = r.mail();
        assert_eq!(
            m.subject,
            "[Modula] Weekly traffic for Personal <&>: 23 Nov – 30 Nov"
        );
        assert!(m.html.contains("Personal &lt;&amp;&gt;"));
        assert!(!m.html.contains("Personal <&>"));
        assert!(m.html.contains("Busiest services"));
        // Every link opens a real page of this workspace.
        assert!(m.html.contains(
            "href=\"https://cloud.example.com/projects/pppppppppppppppp/metrics?env=eeeeeeeeeeeeeeee\""
        ));
        assert!(m.html.contains(
            "href=\"https://cloud.example.com/settings?workspace=wwwwwwwwwwwwwwww#notifications\""
        ));
        assert!(m.text.contains("Report settings: https://cloud.example.com/settings?workspace=wwwwwwwwwwwwwwww#notifications"));
        assert!(m.text.contains(
            "Open in Modula: https://cloud.example.com/projects?workspace=wwwwwwwwwwwwwwww\n"
        ));
        let a = r.alert("aaaaaaaaaaaaaaaa".parse().unwrap());
        assert_eq!(a.phase, Phase::Report);
        assert_eq!(a.subject, "workspace:aaaaaaaaaaaaaaaa");

        let quiet = report(0);
        assert!(quiet.text().contains("No requests reached"));
        assert!(!quiet.mail().html.contains("Busiest services"));
    }
}
