//! HTTP requests served by the nodes' edges (`HttpLogBatch`, from Caddy's
//! access log): every request kept for [`RETENTION_DAYS`] for the
//! dashboard's HTTP tab, and per-minute rollups with a latency histogram
//! for the request charts (rate, errors, p50/p95/p99).

use std::collections::BTreeMap;

use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::Serialize;
use sqlx::PgPool;

use crate::metrics::Range;

pub const RETENTION_DAYS: i32 = 7;
pub const MAX_LIMIT: i64 = 5000;
const MAX_BATCH: usize = 10_000;

/// Upper bounds (ms) of the latency histogram's buckets; one more bucket
/// holds everything slower than the last.
pub const LATENCY_BOUNDS_MS: [u32; 22] = [
    1, 2, 5, 10, 20, 30, 50, 75, 100, 150, 200, 300, 500, 750, 1000, 1500, 2000, 3000, 5000,
    10_000, 30_000, 60_000,
];
pub const BUCKETS: usize = LATENCY_BOUNDS_MS.len() + 1;

/// The histogram bucket of a request that took `us` microseconds.
pub fn bucket_of(us: u32) -> usize {
    LATENCY_BOUNDS_MS
        .iter()
        .position(|&b| u64::from(us) <= u64::from(b) * 1000)
        .unwrap_or(LATENCY_BOUNDS_MS.len())
}

/// The `q` quantile (0–1) of a histogram, in ms: linear within the bucket
/// it falls in; the open last bucket answers its lower bound. None when empty.
pub fn percentile(hist: &[i64], q: f64) -> Option<f64> {
    let total: i64 = hist.iter().sum();
    if total <= 0 {
        return None;
    }
    let target = q.clamp(0.0, 1.0) * total as f64;
    let mut before = 0i64;
    for (i, &n) in hist.iter().enumerate() {
        if n > 0 && (before + n) as f64 >= target {
            let lower = if i == 0 {
                0.0
            } else {
                f64::from(LATENCY_BOUNDS_MS[i - 1])
            };
            let Some(&upper) = LATENCY_BOUNDS_MS.get(i) else {
                return Some(lower);
            };
            let within = ((target - before as f64) / n as f64).clamp(0.0, 1.0);
            return Some(lower + (f64::from(upper) - lower) * within);
        }
        before += n;
    }
    LATENCY_BOUNDS_MS.last().map(|&b| f64::from(b))
}

fn clean(s: &str, max: usize) -> String {
    crate::logs::truncate_line(s, max).replace('\0', "")
}

/// One minute of one deployment's requests.
#[derive(Debug, Default, PartialEq)]
pub struct Rollup {
    pub requests: i32,
    pub status_4xx: i32,
    pub status_5xx: i32,
    pub hist: Vec<i32>,
}

/// A batch's requests per (deployment, minute start in unix seconds).
pub fn roll_up(requests: &[pb::HttpRequestLog]) -> BTreeMap<(&str, i64), Rollup> {
    let mut out: BTreeMap<(&str, i64), Rollup> = BTreeMap::new();
    for r in requests {
        let minute = r.at_unix_ms.div_euclid(60_000) * 60;
        let e = out
            .entry((r.deployment_id.as_str(), minute))
            .or_insert_with(|| Rollup {
                hist: vec![0; BUCKETS],
                ..Rollup::default()
            });
        e.requests += 1;
        match r.status {
            400..=499 => e.status_4xx += 1,
            500..=599 => e.status_5xx += 1,
            _ => {}
        }
        e.hist[bucket_of(r.duration_us)] += 1;
    }
    out
}

/// Store a node's requests. Only deployments with a replica on that node
/// count (a node can't write another's); unknown ones are dropped.
pub async fn record(db: &PgPool, node: Id, b: &pb::HttpLogBatch) -> anyhow::Result<()> {
    if b.dropped > 0 {
        tracing::warn!(%node, dropped = b.dropped, "a node dropped HTTP log lines (buffer full)");
    }
    let reqs = &b.requests[..b.requests.len().min(MAX_BATCH)];
    if reqs.is_empty() {
        return Ok(());
    }
    let mut tx = db.begin().await?;
    sqlx::query(
        "INSERT INTO http_logs (environment_id, service_instance_id, deployment_id, at, host, method,
                                path, status, duration_us, bytes, client_ip, upstream, user_agent)
         SELECT i.environment_id, d.service_instance_id, d.id, to_timestamp(l.at / 1000.0), l.host,
                l.method, l.path, l.status, l.dur, l.bytes, l.ip, l.up, l.ua
         FROM unnest($2::text[], $3::bigint[], $4::text[], $5::text[], $6::text[], $7::int2[],
                     $8::int4[], $9::int8[], $10::text[], $11::text[], $12::text[])
              AS l(dep, at, host, method, path, status, dur, bytes, ip, up, ua)
         JOIN deployments d ON d.id = l.dep::text
         JOIN service_instances i ON i.id = d.service_instance_id
         WHERE EXISTS (SELECT 1 FROM deployment_replicas r WHERE r.deployment_id = d.id AND r.node_id = $1)",
    )
    .bind(node)
    .bind(reqs.iter().map(|r| r.deployment_id.clone()).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| r.at_unix_ms).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| clean(&r.host, 255)).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| clean(&r.method, 16)).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| clean(&r.path, 2048)).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| r.status.min(999) as i16).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| r.duration_us.min(i32::MAX as u32) as i32).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| r.bytes.min(i64::MAX as u64) as i64).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| clean(&r.client_ip, 64)).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| clean(&r.upstream, 64)).collect::<Vec<_>>())
    .bind(reqs.iter().map(|r| clean(&r.user_agent, 512)).collect::<Vec<_>>())
    .execute(&mut *tx)
    .await?;
    for ((deployment, minute), r) in roll_up(reqs) {
        sqlx::query(
            "INSERT INTO http_rollups (deployment_id, service_instance_id, minute, requests,
                                       status_4xx, status_5xx, latency_hist)
             SELECT d.id, d.service_instance_id, to_timestamp($3), $4, $5, $6, $7
             FROM deployments d
             WHERE d.id = $1
               AND EXISTS (SELECT 1 FROM deployment_replicas r WHERE r.deployment_id = d.id AND r.node_id = $2)
             ON CONFLICT (deployment_id, minute) DO UPDATE SET
                 requests = http_rollups.requests + EXCLUDED.requests,
                 status_4xx = http_rollups.status_4xx + EXCLUDED.status_4xx,
                 status_5xx = http_rollups.status_5xx + EXCLUDED.status_5xx,
                 latency_hist = ARRAY(
                     SELECT COALESCE(a, 0) + COALESCE(b, 0)
                     FROM unnest(http_rollups.latency_hist, EXCLUDED.latency_hist)
                          WITH ORDINALITY AS t(a, b, n)
                     ORDER BY n)",
        )
        .bind(deployment)
        .bind(node)
        .bind(minute as f64)
        .bind(r.requests)
        .bind(r.status_4xx)
        .bind(r.status_5xx)
        .bind(&r.hist)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Drop requests and rollups older than the retention.
pub async fn purge(db: &PgPool) -> anyhow::Result<u64> {
    let a = sqlx::query("DELETE FROM http_logs WHERE at < now() - make_interval(days => $1)")
        .bind(RETENTION_DAYS)
        .execute(db)
        .await?;
    let b =
        sqlx::query("DELETE FROM http_rollups WHERE minute < now() - make_interval(days => $1)")
            .bind(RETENTION_DAYS)
            .execute(db)
            .await?;
    Ok(a.rows_affected() + b.rows_affected())
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub since_ms: i64,
    pub until_ms: i64,
    pub service: Option<Id>,
    /// 2–5: only 2xx … 5xx responses.
    pub status_class: Option<i32>,
    pub method: Option<String>,
    /// Case-insensitive substring of the path.
    pub path: Option<String>,
    pub limit: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub t: i64,
    pub service_id: Id,
    pub service_name: String,
    pub deployment_id: Id,
    pub host: String,
    pub method: String,
    pub path: String,
    pub status: i32,
    pub duration_ms: f64,
    pub bytes: i64,
    pub client_ip: String,
    pub upstream: String,
    pub user_agent: String,
}

#[derive(Debug, Serialize, sqlx::FromRow, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Bucket {
    pub t: i64,
    pub count: i64,
    pub status_4xx: i64,
    pub status_5xx: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    /// Oldest first; when truncated, the newest `limit` requests.
    pub requests: Vec<Request>,
    pub histogram: Vec<Bucket>,
    pub bucket_ms: i64,
    pub truncated: bool,
}

pub async fn search(db: &PgPool, environment: Id, q: &Query) -> sqlx::Result<Found> {
    let path = q
        .path
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(crate::logs::like_pattern);
    let limit = q.limit.clamp(1, MAX_LIMIT);
    let mut requests: Vec<Request> = sqlx::query_as(
        "SELECT (extract(epoch FROM h.at) * 1000)::bigint AS t, s.id AS service_id,
                s.name AS service_name, h.deployment_id, h.host, h.method, h.path,
                h.status::int4 AS status, h.duration_us / 1000.0::float8 AS duration_ms, h.bytes,
                h.client_ip, h.upstream, h.user_agent
         FROM http_logs h
         JOIN service_instances i ON i.id = h.service_instance_id
         JOIN services s ON s.id = i.service_id
         WHERE h.environment_id = $1
           AND h.at >= to_timestamp($2 / 1000.0) AND h.at < to_timestamp($3 / 1000.0)
           AND ($4::text IS NULL OR s.id = $4)
           AND ($5::int4 IS NULL OR h.status / 100 = $5)
           AND ($6::text IS NULL OR h.method = $6)
           AND ($7::text IS NULL OR h.path ILIKE $7)
         ORDER BY h.at DESC, h.ctid DESC
         LIMIT $8",
    )
    .bind(environment)
    .bind(q.since_ms as f64)
    .bind(q.until_ms as f64)
    .bind(q.service)
    .bind(q.status_class)
    .bind(&q.method)
    .bind(&path)
    .bind(limit + 1)
    .fetch_all(db)
    .await?;
    let truncated = requests.len() as i64 > limit;
    requests.truncate(limit as usize);
    requests.reverse();

    let step = crate::logs::bucket_ms(q.since_ms, q.until_ms);
    let counted: Vec<Bucket> = sqlx::query_as(
        "SELECT ($2 + floor(((extract(epoch FROM h.at) * 1000) - $2) / $4)::bigint * $4)::bigint AS t,
                count(*) AS count,
                count(*) FILTER (WHERE h.status BETWEEN 400 AND 499) AS status_4xx,
                count(*) FILTER (WHERE h.status >= 500) AS status_5xx
         FROM http_logs h
         JOIN service_instances i ON i.id = h.service_instance_id
         WHERE h.environment_id = $1
           AND h.at >= to_timestamp($2 / 1000.0) AND h.at < to_timestamp($3 / 1000.0)
           AND ($5::text IS NULL OR i.service_id = $5)
           AND ($6::int4 IS NULL OR h.status / 100 = $6)
           AND ($7::text IS NULL OR h.method = $7)
           AND ($8::text IS NULL OR h.path ILIKE $8)
         GROUP BY 1 ORDER BY 1",
    )
    .bind(environment)
    .bind(q.since_ms)
    .bind(q.until_ms)
    .bind(step)
    .bind(q.service)
    .bind(q.status_class)
    .bind(&q.method)
    .bind(&path)
    .fetch_all(db)
    .await?;
    Ok(Found {
        requests,
        histogram: fill(q.since_ms, q.until_ms, step, counted),
        bucket_ms: step,
        truncated,
    })
}

fn fill(since_ms: i64, until_ms: i64, step: i64, counted: Vec<Bucket>) -> Vec<Bucket> {
    let mut out = Vec::new();
    let mut it = counted.into_iter().peekable();
    let mut t = since_ms;
    while t < until_ms {
        match it.peek() {
            Some(b) if b.t == t => out.push(it.next().expect("peeked")),
            _ => out.push(Bucket {
                t,
                count: 0,
                status_4xx: 0,
                status_5xx: 0,
            }),
        }
        t += step;
    }
    out
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    /// Bucket start, unix ms.
    pub t: i64,
    pub requests: i64,
    pub status_4xx: i64,
    pub status_5xx: i64,
    pub p50_ms: Option<f64>,
    pub p95_ms: Option<f64>,
    pub p99_ms: Option<f64>,
}

#[derive(sqlx::FromRow)]
pub struct MinuteRow {
    /// Unix seconds.
    pub minute: i64,
    pub requests: i32,
    pub status_4xx: i32,
    pub status_5xx: i32,
    pub latency_hist: Vec<i32>,
}

/// Minute rows summed into buckets of `step` seconds, every bucket of the
/// last `span` seconds before `now` (empty ones too). Percentiles come from
/// the summed histograms, never from averaging per-minute percentiles.
pub fn aggregate(rows: &[MinuteRow], now: i64, span: i64, step: i64) -> Vec<Point> {
    let first = (now - span).div_euclid(step) + 1;
    let last = now.div_euclid(step);
    let n = (last - first + 1).max(0) as usize;
    let mut acc: Vec<(i64, i64, i64, Vec<i64>)> = vec![(0, 0, 0, vec![0; BUCKETS]); n];
    for r in rows {
        let b = r.minute.div_euclid(step) - first;
        let Some(a) = usize::try_from(b).ok().and_then(|b| acc.get_mut(b)) else {
            continue;
        };
        a.0 += i64::from(r.requests);
        a.1 += i64::from(r.status_4xx);
        a.2 += i64::from(r.status_5xx);
        for (s, v) in a.3.iter_mut().zip(&r.latency_hist) {
            *s += i64::from(*v);
        }
    }
    acc.into_iter()
        .enumerate()
        .map(|(i, (requests, s4, s5, hist))| Point {
            t: (first + i as i64) * step * 1000,
            requests,
            status_4xx: s4,
            status_5xx: s5,
            p50_ms: percentile(&hist, 0.50),
            p95_ms: percentile(&hist, 0.95),
            p99_ms: percentile(&hist, 0.99),
        })
        .collect()
}

/// Several instances' requests over `range` (one instance, or a project's
/// in one environment).
pub async fn series(db: &PgPool, instances: &[Id], range: Range) -> sqlx::Result<Vec<Point>> {
    let (span, step) = range.span_and_step();
    let now: i64 = sqlx::query_scalar("SELECT extract(epoch FROM now())::bigint")
        .fetch_one(db)
        .await?;
    let rows: Vec<MinuteRow> = sqlx::query_as(
        "SELECT extract(epoch FROM minute)::bigint AS minute, requests, status_4xx, status_5xx,
                latency_hist
         FROM http_rollups
         WHERE service_instance_id = ANY($1) AND minute > now() - make_interval(secs => $2)",
    )
    .bind(instances)
    .bind(span as f64)
    .fetch_all(db)
    .await?;
    Ok(aggregate(&rows, now, span, step))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(dep: &str, at_unix_ms: i64, status: u32, ms: u32) -> pb::HttpRequestLog {
        pb::HttpRequestLog {
            deployment_id: dep.into(),
            at_unix_ms,
            status,
            duration_us: ms * 1000,
            ..Default::default()
        }
    }

    #[test]
    fn latency_buckets() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(1000), 0);
        assert_eq!(bucket_of(1001), 1);
        assert_eq!(bucket_of(100_000), 8);
        assert_eq!(bucket_of(u32::MAX), BUCKETS - 1);
    }

    #[test]
    fn percentiles_interpolate_within_a_bucket() {
        let mut h = vec![0i64; BUCKETS];
        assert_eq!(percentile(&h, 0.95), None);
        // 100 requests between 50 and 75 ms: p50 is halfway through.
        h[bucket_of(60_000)] = 100;
        assert_eq!(percentile(&h, 0.5), Some(62.5));
        assert_eq!(percentile(&h, 0.95), Some(73.75));
        // Slower than the last bound: its lower bound.
        let mut slow = vec![0i64; BUCKETS];
        slow[BUCKETS - 1] = 3;
        assert_eq!(percentile(&slow, 0.99), Some(60_000.0));
    }

    /// The p95 of a bucket comes from the merged histogram: two minutes, one
    /// with 95 fast requests and one with 5 slow ones, have a p95 near the
    /// fast ones, not the average of each minute's p95 (~500 ms).
    #[test]
    fn rollups_merge_into_true_percentiles() {
        let t0 = 1_800_000_000_000i64; // a minute boundary
        let mut reqs: Vec<_> = (0..95).map(|i| req("d1", t0 + i, 200, 8)).collect();
        reqs.extend((0..4).map(|i| req("d1", t0 + 60_000 + i, 503, 900)));
        reqs.push(req("d1", t0 + 60_000 + 10, 404, 900));
        reqs.push(req("d2", t0, 200, 3));
        let rolled = roll_up(&reqs);
        assert_eq!(rolled.len(), 3);
        let a = &rolled[&("d1", t0 / 1000)];
        assert_eq!((a.requests, a.status_4xx, a.status_5xx), (95, 0, 0));
        let b = &rolled[&("d1", t0 / 1000 + 60)];
        assert_eq!((b.requests, b.status_4xx, b.status_5xx), (5, 1, 4));
        let rows: Vec<MinuteRow> = [a, b]
            .iter()
            .enumerate()
            .map(|(i, r)| MinuteRow {
                minute: t0 / 1000 + 60 * i as i64,
                requests: r.requests,
                status_4xx: r.status_4xx,
                status_5xx: r.status_5xx,
                latency_hist: r.hist.clone(),
            })
            .collect();
        // One 5-minute bucket holding both minutes.
        let now = t0 / 1000 + 120;
        let points = aggregate(&rows, now, 600, 300);
        assert_eq!(points.len(), 2);
        let p = points.iter().find(|p| p.requests > 0).unwrap();
        assert_eq!((p.requests, p.status_4xx, p.status_5xx), (100, 1, 4));
        let p95 = p.p95_ms.unwrap();
        assert!((5.0..=10.0).contains(&p95), "p95 {p95}");
        assert!(p.p99_ms.unwrap() >= 750.0);
        // Empty buckets are there, without percentiles.
        let empty = points.iter().find(|p| p.requests == 0).unwrap();
        assert_eq!(empty.p50_ms, None);
    }

    #[test]
    fn every_bucket_of_the_range_is_present() {
        let points = aggregate(&[], 3600 * 10 + 30, 3600, 60);
        assert_eq!(points.len(), 60);
        assert_eq!(points.last().unwrap().t, 3600 * 10 * 1000);
        assert!(points.windows(2).all(|w| w[1].t - w[0].t == 60_000));
    }
}
