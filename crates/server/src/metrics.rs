//! Resource usage of deployments and of the servers themselves (CPU, memory,
//! network; disk for servers): samples the nodes send every 30s, kept for
//! [`RETENTION_DAYS`], and read back averaged into buckets for the
//! dashboard's charts and `modula metrics`.

use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

pub const RETENTION_DAYS: i32 = 7;

/// A number from a node, stored as is unless negative or not finite (0).
fn finite(v: f64) -> f64 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

/// Store a node's samples: its own usage, and its workloads'. Only
/// deployments placed on that node count (a node can't write another node's
/// numbers); unknown ones are dropped.
pub async fn record(db: &PgPool, node: Id, r: &pb::MetricsReport) -> anyhow::Result<()> {
    if let Some(h) = &r.host {
        record_host(db, node, h).await?;
    }
    let w = &r.workloads[..r.workloads.len().min(512)];
    if w.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO metric_samples (deployment_id, service_instance_id, at, cpu_percent, memory_bytes, net_rx_bps, net_tx_bps)
         SELECT d.id, d.service_instance_id, to_timestamp(m.at / 1000.0), m.cpu, m.mem, m.rx, m.tx
         FROM unnest($2::text[], $3::bigint[], $4::float4[], $5::bigint[], $6::float4[], $7::float4[])
              AS m(dep, at, cpu, mem, rx, tx)
         JOIN deployments d ON d.id = m.dep::text AND d.node_id = $1",
    )
    .bind(node)
    .bind(w.iter().map(|m| m.deployment_id.clone()).collect::<Vec<_>>())
    .bind(w.iter().map(|m| m.at_unix_ms).collect::<Vec<_>>())
    .bind(w.iter().map(|m| finite(m.cpu_percent) as f32).collect::<Vec<_>>())
    .bind(w.iter().map(|m| m.memory_bytes.min(i64::MAX as u64) as i64).collect::<Vec<_>>())
    .bind(w.iter().map(|m| finite(m.net_rx_bytes_per_sec) as f32).collect::<Vec<_>>())
    .bind(w.iter().map(|m| finite(m.net_tx_bytes_per_sec) as f32).collect::<Vec<_>>())
    .execute(db)
    .await?;
    Ok(())
}

/// Store the server's own usage, at the time the node measured it unless
/// that is more than 5 minutes off this server's clock (then now).
async fn record_host(db: &PgPool, node: Id, h: &pb::HostStats) -> sqlx::Result<()> {
    let bytes = |v: u64| v.min(i64::MAX as u64) as i64;
    sqlx::query(
        "INSERT INTO node_samples (node_id, at, cpu_percent, memory_used_bytes, memory_total_bytes,
                                   load1, net_rx_bps, net_tx_bps)
         VALUES ($1, CASE WHEN abs(extract(epoch FROM now()) * 1000 - $2::bigint) < 300000
                          THEN to_timestamp($2::bigint / 1000.0) ELSE now() END,
                 $3, $4, $5, $6, $7, $8)",
    )
    .bind(node)
    .bind(h.at_unix_ms)
    .bind(finite(h.cpu_percent).min(100.0) as f32)
    .bind(bytes(h.memory_used_bytes))
    .bind(bytes(h.memory_total_bytes))
    .bind(finite(h.load1) as f32)
    .bind(finite(h.net_rx_bytes_per_sec) as f32)
    .bind(finite(h.net_tx_bytes_per_sec) as f32)
    .execute(db)
    .await?;
    Ok(())
}

/// Drop samples older than the retention.
pub async fn purge(db: &PgPool) -> anyhow::Result<u64> {
    let r = sqlx::query("DELETE FROM metric_samples WHERE at < now() - make_interval(days => $1)")
        .bind(RETENTION_DAYS)
        .execute(db)
        .await?;
    Ok(r.rows_affected())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum Range {
    #[default]
    #[serde(rename = "1h")]
    Hour,
    #[serde(rename = "6h")]
    SixHours,
    #[serde(rename = "24h")]
    Day,
    #[serde(rename = "7d")]
    Week,
}

impl Range {
    /// (span, bucket) in seconds: about 60–170 points per chart.
    pub fn span_and_step(self) -> (i64, i64) {
        match self {
            Range::Hour => (3600, 60),
            Range::SixHours => (6 * 3600, 300),
            Range::Day => (24 * 3600, 900),
            Range::Week => (7 * 24 * 3600, 3600),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    /// Bucket start, unix ms.
    pub t: i64,
    pub cpu_percent: f64,
    pub memory_bytes: i64,
    pub net_rx_bytes_per_sec: f64,
    pub net_tx_bytes_per_sec: f64,
}

/// The instance's usage over `range`: per bucket, each deployment's average,
/// summed (two deployments overlap while one replaces the other).
pub async fn series(db: &PgPool, instance: Id, range: Range) -> sqlx::Result<Vec<Point>> {
    series_of(db, &[instance], range).await
}

/// Several instances' usage summed (a project's in one environment).
pub async fn series_of(db: &PgPool, instances: &[Id], range: Range) -> sqlx::Result<Vec<Point>> {
    let (span, step) = range.span_and_step();
    sqlx::query_as(
        "WITH per AS (
             SELECT floor(extract(epoch FROM at) / $3)::bigint AS b, deployment_id,
                    avg(cpu_percent) AS cpu, avg(memory_bytes) AS mem,
                    avg(net_rx_bps) AS rx, avg(net_tx_bps) AS tx
             FROM metric_samples
             WHERE service_instance_id = ANY($1) AND at > now() - make_interval(secs => $2)
             GROUP BY 1, 2
         )
         SELECT b * $3 * 1000 AS t, sum(cpu)::float8 AS cpu_percent, sum(mem)::bigint AS memory_bytes,
                sum(rx)::float8 AS net_rx_bytes_per_sec, sum(tx)::float8 AS net_tx_bytes_per_sec
         FROM per GROUP BY b ORDER BY b",
    )
    .bind(instances)
    .bind(span as f64)
    .bind(step)
    .fetch_all(db)
    .await
}

#[derive(Debug, Clone, Serialize, PartialEq, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct NodePoint {
    /// Bucket start, unix ms.
    pub t: i64,
    /// Busy share of all cores (100 = every core).
    pub cpu_percent: f64,
    pub memory_used_bytes: i64,
    pub memory_total_bytes: i64,
    pub load1: f64,
    pub net_rx_bytes_per_sec: f64,
    pub net_tx_bytes_per_sec: f64,
}

/// A server's own usage over `range`, averaged per bucket.
pub async fn node_series(db: &PgPool, node: Id, range: Range) -> sqlx::Result<Vec<NodePoint>> {
    let (span, step) = range.span_and_step();
    sqlx::query_as(
        "SELECT floor(extract(epoch FROM at) / $3)::bigint * $3 * 1000 AS t,
                avg(cpu_percent)::float8 AS cpu_percent,
                avg(memory_used_bytes)::bigint AS memory_used_bytes,
                max(memory_total_bytes) AS memory_total_bytes,
                avg(load1)::float8 AS load1,
                avg(net_rx_bps)::float8 AS net_rx_bytes_per_sec,
                avg(net_tx_bps)::float8 AS net_tx_bytes_per_sec
         FROM node_samples
         WHERE node_id = $1 AND at > now() - make_interval(secs => $2)
         GROUP BY 1 ORDER BY 1",
    )
    .bind(node)
    .bind(span as f64)
    .bind(step)
    .fetch_all(db)
    .await
}

/// Disk samples come every 5 minutes: buckets are never shorter.
pub const DISK_MIN_STEP_SECS: i64 = 300;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DiskSeries {
    /// The directory the node measured (its data dir, or Docker's when that
    /// is on another filesystem).
    pub path: String,
    pub points: Vec<DiskPoint>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DiskPoint {
    /// Bucket start, unix ms.
    pub t: i64,
    pub used_bytes: i64,
    pub total_bytes: i64,
}

/// A server's filesystems over `range`, one series each, in buckets of at
/// least [`DISK_MIN_STEP_SECS`].
pub async fn disk_series(db: &PgPool, node: Id, range: Range) -> sqlx::Result<Vec<DiskSeries>> {
    let (span, step) = range.span_and_step();
    let rows: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT path, floor(extract(epoch FROM at) / $3)::bigint * $3 * 1000 AS t,
                avg(total_bytes - free_bytes)::bigint, max(total_bytes)
         FROM disk_samples
         WHERE node_id = $1 AND at > now() - make_interval(secs => $2)
         GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .bind(node)
    .bind(span as f64)
    .bind(step.max(DISK_MIN_STEP_SECS))
    .fetch_all(db)
    .await?;
    let mut out: Vec<DiskSeries> = Vec::new();
    for (path, t, used_bytes, total_bytes) in rows {
        let point = DiskPoint {
            t,
            used_bytes,
            total_bytes,
        };
        match out.last_mut() {
            Some(s) if s.path == path => s.points.push(point),
            _ => out.push(DiskSeries {
                path,
                points: vec![point],
            }),
        }
    }
    Ok(out)
}

/// Store a node's disk report: its filesystems (for disk alerts) and the
/// size of its volumes (only volumes of instances placed on that node).
pub async fn record_disk(db: &PgPool, node: Id, r: &pb::DiskReport) -> anyhow::Result<()> {
    let fs = &r.filesystems[..r.filesystems.len().min(8)];
    if !fs.is_empty() {
        sqlx::query(
            "INSERT INTO disk_samples (node_id, path, total_bytes, free_bytes)
             SELECT $1, f.path, f.total, f.free
             FROM unnest($2::text[], $3::bigint[], $4::bigint[]) AS f(path, total, free)",
        )
        .bind(node)
        .bind(
            fs.iter()
                .map(|f| f.path.chars().take(512).collect::<String>())
                .collect::<Vec<_>>(),
        )
        .bind(
            fs.iter()
                .map(|f| f.total_bytes.min(i64::MAX as u64) as i64)
                .collect::<Vec<_>>(),
        )
        .bind(
            fs.iter()
                .map(|f| f.free_bytes.min(i64::MAX as u64) as i64)
                .collect::<Vec<_>>(),
        )
        .execute(db)
        .await?;
    }
    let vols: Vec<(String, i64)> = r.volumes[..r.volumes.len().min(512)]
        .iter()
        .filter_map(|v| {
            let id = v.name.strip_prefix("modula-vol-")?;
            Some((id.to_owned(), v.used_bytes.min(i64::MAX as u64) as i64))
        })
        .collect();
    if !vols.is_empty() {
        sqlx::query(
            "INSERT INTO volume_usage (volume_id, node_id, used_bytes, at)
             SELECT v.id, $1, u.used, now()
             FROM unnest($2::text[], $3::bigint[]) AS u(vol, used)
             JOIN volumes v ON v.id = u.vol
             JOIN service_instances i ON i.id = v.service_instance_id AND i.node_id = $1
             ON CONFLICT (volume_id) DO UPDATE
                 SET node_id = EXCLUDED.node_id, used_bytes = EXCLUDED.used_bytes, at = now()",
        )
        .bind(node)
        .bind(vols.iter().map(|v| v.0.clone()).collect::<Vec<_>>())
        .bind(vols.iter().map(|v| v.1).collect::<Vec<_>>())
        .execute(db)
        .await?;
    }
    Ok(())
}

/// Record, for the current minute, whether each service that should be
/// running is (called every minute by the alert monitor; a second call in the
/// same minute is a no-op). Judged on the serving deployment: up while one of
/// its replicas is active on a reachable server, down while it is crashed or
/// all its servers are offline. Nothing is written for services with nothing
/// to run (not deployed, sleeping, stopped), cron jobs (their container only
/// runs on schedule) or deployments hosted elsewhere (not observable), nor
/// while no server is reachable at all (e.g. right after this one restarts).
pub async fn record_uptime(db: &PgPool) -> sqlx::Result<u64> {
    let r = sqlx::query(
        "INSERT INTO uptime_minutes (service_instance_id, minute, up, reason)
         SELECT DISTINCT ON (d.service_instance_id)
                d.service_instance_id, date_trunc('minute', now()), s.up, s.reason
         FROM deployments d
         CROSS JOIN LATERAL (
             SELECT coalesce(bool_or(r.status = 'active' AND x.reach), false) AS up,
                    coalesce(bool_or(x.reach), false) AS any_reach,
                    coalesce(bool_or(r.status IN ('crashed', 'failed')), false) AS any_crash
             FROM deployment_replicas r
             LEFT JOIN nodes n ON n.id = r.node_id
             CROSS JOIN LATERAL (
                 SELECT coalesce(n.status = 'online' AND n.last_seen_at > now() - interval '90 seconds', false)
                     AS reach
             ) x
             WHERE r.deployment_id = d.id
         ) rs
         CROSS JOIN LATERAL (
             SELECT d.status = 'active' AND rs.up AS up,
                    CASE WHEN d.status = 'crashed' THEN 'crashed'
                         WHEN rs.up THEN NULL
                         WHEN NOT rs.any_reach THEN 'unreachable'
                         WHEN rs.any_crash THEN 'crashed'
                         ELSE 'restarting' END AS reason
         ) s
         WHERE d.status IN ('active', 'crashed')
           AND d.config->'hosting' IS NULL AND d.config->>'cronSchedule' IS NULL
           -- Seeing no server at all is this server just started (or cut
           -- off), not every service down: record nothing.
           AND EXISTS (SELECT 1 FROM nodes
                       WHERE status = 'online' AND last_seen_at > now() - interval '90 seconds')
         ORDER BY d.service_instance_id, (d.status = 'active') DESC, d.created_at DESC
         ON CONFLICT DO NOTHING",
    )
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

/// Drop uptime rows older than the retention.
pub async fn purge_uptime(db: &PgPool) -> sqlx::Result<u64> {
    let r =
        sqlx::query("DELETE FROM uptime_minutes WHERE minute < now() - make_interval(days => $1)")
            .bind(RETENTION_DAYS)
            .execute(db)
            .await?;
    Ok(r.rows_affected())
}

impl Range {
    /// (span, bucket) in seconds for the uptime strip: 48–84 bars, each a
    /// whole number of the charts' buckets (the 5xx strip regroups those).
    pub fn uptime_step(self) -> (i64, i64) {
        match self {
            Range::Hour => (3600, 60),
            Range::SixHours => (6 * 3600, 300),
            Range::Day => (24 * 3600, 1800),
            Range::Week => (7 * 24 * 3600, 7200),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UptimeState {
    Up,
    Down,
    /// Nothing was meant to run (or no record of it).
    None,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UptimeBucket {
    /// Bucket start, unix ms.
    pub t: i64,
    pub state: UptimeState,
    pub down_ms: i64,
    /// Why it was down ("crashed", "unreachable", "restarting").
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Uptime {
    pub step_secs: i64,
    /// Share of the recorded minutes that were up; null without any.
    pub uptime: Option<f64>,
    pub buckets: Vec<UptimeBucket>,
}

/// The instances' uptime over `range`, every bucket present (oldest first,
/// the last one holding now). Several instances (a project's): a bucket is
/// down if any was down, `down_ms` counts the minutes any was.
pub async fn uptime_of(db: &PgPool, instances: &[Id], range: Range) -> sqlx::Result<Uptime> {
    let (span, step) = range.uptime_step();
    let n = span / step;
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(db)
        .await?;
    let first = now.div_euclid(step) - n + 1;
    let rows: Vec<(i64, i64, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT floor(extract(epoch FROM minute) / $3)::bigint AS b,
                count(*) FILTER (WHERE up),
                count(*) FILTER (WHERE NOT up),
                count(DISTINCT minute) FILTER (WHERE NOT up),
                mode() WITHIN GROUP (ORDER BY reason)
         FROM uptime_minutes
         WHERE service_instance_id = ANY($1) AND minute >= to_timestamp($2)
         GROUP BY 1",
    )
    .bind(instances)
    .bind((first * step) as f64)
    .bind(step)
    .fetch_all(db)
    .await?;
    Ok(fill_uptime(first, n, step, &rows))
}

/// Every bucket from `first` (bucket number) on, from per-bucket counts
/// (bucket, up rows, down rows, distinct down minutes, reason).
fn fill_uptime(
    first: i64,
    n: i64,
    step: i64,
    rows: &[(i64, i64, i64, i64, Option<String>)],
) -> Uptime {
    let (mut up, mut total) = (0i64, 0i64);
    let buckets = (first..first + n)
        .map(|b| {
            let row = rows.iter().find(|r| r.0 == b);
            let (u, d, dm, reason) = row.map_or((0, 0, 0, None), |r| (r.1, r.2, r.3, r.4.clone()));
            up += u;
            total += u + d;
            let state = match (u, d) {
                (_, d) if d > 0 => UptimeState::Down,
                (u, _) if u > 0 => UptimeState::Up,
                _ => UptimeState::None,
            };
            UptimeBucket {
                t: b * step * 1000,
                state,
                down_ms: dm * 60_000,
                reason: if d > 0 { reason } else { None },
            }
        })
        .collect();
    Uptime {
        step_secs: step,
        uptime: (total > 0).then(|| up as f64 / total as f64),
        buckets,
    }
}

#[cfg(test)]
mod uptime_tests {
    use super::*;

    #[test]
    fn buckets_are_filled_and_ratio_ignores_empty_minutes() {
        let rows = vec![
            (11, 20, 0, 0, None),
            (12, 16, 4, 4, Some("crashed".to_owned())),
        ];
        let u = fill_uptime(10, 4, 1200, &rows);
        let states: Vec<_> = u.buckets.iter().map(|b| b.state).collect();
        assert_eq!(
            states,
            [
                UptimeState::None,
                UptimeState::Up,
                UptimeState::Down,
                UptimeState::None
            ]
        );
        assert_eq!(u.buckets[0].t, 10 * 1200 * 1000);
        assert_eq!(u.buckets[2].down_ms, 4 * 60_000);
        assert_eq!(u.buckets[2].reason.as_deref(), Some("crashed"));
        assert_eq!(u.uptime, Some(36.0 / 40.0));
        assert_eq!(fill_uptime(0, 3, 60, &[]).uptime, None);
    }
}
