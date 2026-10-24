//! Resource usage of deployments (CPU, memory, network): samples the nodes
//! send every 30s, kept for [`RETENTION_DAYS`], and read back averaged into
//! buckets for the dashboard's charts and `modula metrics`.

use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

pub const RETENTION_DAYS: i32 = 7;

/// Store a node's samples. Only deployments placed on that node count (a
/// node can't write another node's numbers); unknown ones are dropped.
pub async fn record(db: &PgPool, node: Id, r: &pb::MetricsReport) -> anyhow::Result<()> {
    let w = &r.workloads[..r.workloads.len().min(512)];
    if w.is_empty() {
        return Ok(());
    }
    let finite = |v: f64| if v.is_finite() { v.max(0.0) } else { 0.0 };
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
