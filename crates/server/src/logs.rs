//! Workload output the nodes ship as it happens (`LogBatch`), kept for
//! [`RETENTION_DAYS`] and searched per environment for the dashboard's Logs
//! page. Deployment log tabs still stream from the node; this store is what
//! survives the container.

use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde::Serialize;
use sqlx::PgPool;

pub const RETENTION_DAYS: i32 = 7;
pub const MAX_LIMIT: i64 = 5000;
/// Histogram buckets per query.
const BUCKETS: i64 = 60;

/// Store a node's lines. Only deployments placed on that node count (a node
/// can't write another node's logs); unknown ones are dropped.
pub async fn record(db: &PgPool, node: Id, b: &pb::LogBatch) -> anyhow::Result<()> {
    if b.dropped > 0 {
        tracing::warn!(%node, dropped = b.dropped, "a node dropped log lines (buffer full)");
    }
    let lines = &b.lines[..b.lines.len().min(10_000)];
    if lines.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO log_lines (environment_id, service_instance_id, deployment_id, at, stderr, line)
         SELECT i.environment_id, d.service_instance_id, d.id, to_timestamp(l.at / 1000.0), l.err, l.line
         FROM unnest($2::text[], $3::bigint[], $4::bool[], $5::text[]) AS l(dep, at, err, line)
         JOIN deployments d ON d.id = l.dep::text AND d.node_id = $1
         JOIN service_instances i ON i.id = d.service_instance_id",
    )
    .bind(node)
    .bind(lines.iter().map(|l| l.deployment_id.clone()).collect::<Vec<_>>())
    .bind(lines.iter().map(|l| l.at_unix_ms).collect::<Vec<_>>())
    .bind(lines.iter().map(|l| l.stderr).collect::<Vec<_>>())
    // Postgres text can't hold NUL.
    .bind(lines.iter().map(|l| l.text.replace('\0', "")).collect::<Vec<_>>())
    .execute(db)
    .await?;
    Ok(())
}

/// Drop lines older than the retention.
pub async fn purge(db: &PgPool) -> anyhow::Result<u64> {
    let r = sqlx::query("DELETE FROM log_lines WHERE at < now() - make_interval(days => $1)")
        .bind(RETENTION_DAYS)
        .execute(db)
        .await?;
    Ok(r.rows_affected())
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub since_ms: i64,
    pub until_ms: i64,
    /// Case-insensitive substring.
    pub text: Option<String>,
    pub service: Option<Id>,
    pub limit: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Line {
    pub t: i64,
    pub service_id: Id,
    pub service_name: String,
    pub deployment_id: Id,
    pub stream: String,
    pub line: String,
}

#[derive(Debug, Serialize, sqlx::FromRow, PartialEq)]
pub struct Bucket {
    pub t: i64,
    pub count: i64,
    pub errors: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    /// Oldest first; when truncated, the newest `limit` lines.
    pub lines: Vec<Line>,
    pub histogram: Vec<Bucket>,
    pub bucket_ms: i64,
    pub truncated: bool,
}

/// `ILIKE` pattern matching `text` literally.
fn like_pattern(text: &str) -> String {
    let mut p = String::with_capacity(text.len() + 2);
    p.push('%');
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            p.push('\\');
        }
        p.push(c);
    }
    p.push('%');
    p
}

/// Whole seconds, about [`BUCKETS`] of them over the range (rounded, so a
/// range a few ms over 15 minutes still gets 15 s buckets).
pub fn bucket_ms(since_ms: i64, until_ms: i64) -> i64 {
    let span = (until_ms - since_ms).max(1000);
    ((span / BUCKETS + 500) / 1000).max(1) * 1000
}

pub async fn search(db: &PgPool, environment: Id, q: &Query) -> sqlx::Result<Found> {
    let pattern = q
        .text
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(like_pattern);
    let limit = q.limit.clamp(1, MAX_LIMIT);
    let mut lines: Vec<Line> = sqlx::query_as(
        "SELECT (extract(epoch FROM l.at) * 1000)::bigint AS t, s.id AS service_id,
                s.name AS service_name, l.deployment_id,
                CASE WHEN l.stderr THEN 'stderr' ELSE 'stdout' END AS stream, l.line
         FROM log_lines l
         JOIN service_instances i ON i.id = l.service_instance_id
         JOIN services s ON s.id = i.service_id
         WHERE l.environment_id = $1
           AND l.at >= to_timestamp($2 / 1000.0) AND l.at < to_timestamp($3 / 1000.0)
           AND ($4::text IS NULL OR l.line ILIKE $4)
           AND ($5::text IS NULL OR s.id = $5)
         ORDER BY l.at DESC, l.ctid DESC
         LIMIT $6",
    )
    .bind(environment)
    .bind(q.since_ms as f64)
    .bind(q.until_ms as f64)
    .bind(&pattern)
    .bind(q.service)
    .bind(limit + 1)
    .fetch_all(db)
    .await?;
    let truncated = lines.len() as i64 > limit;
    lines.truncate(limit as usize);
    lines.reverse();

    let step = bucket_ms(q.since_ms, q.until_ms);
    let counted: Vec<Bucket> = sqlx::query_as(
        "SELECT ($2 + floor(((extract(epoch FROM l.at) * 1000) - $2) / $4)::bigint * $4)::bigint AS t,
                count(*) AS count, count(*) FILTER (WHERE l.stderr) AS errors
         FROM log_lines l
         JOIN service_instances i ON i.id = l.service_instance_id
         WHERE l.environment_id = $1
           AND l.at >= to_timestamp($2 / 1000.0) AND l.at < to_timestamp($3 / 1000.0)
           AND ($5::text IS NULL OR l.line ILIKE $5)
           AND ($6::text IS NULL OR i.service_id = $6)
         GROUP BY 1 ORDER BY 1",
    )
    .bind(environment)
    .bind(q.since_ms)
    .bind(q.until_ms)
    .bind(step)
    .bind(&pattern)
    .bind(q.service)
    .fetch_all(db)
    .await?;
    Ok(Found {
        lines,
        histogram: fill(q.since_ms, q.until_ms, step, counted),
        bucket_ms: step,
        truncated,
    })
}

/// Every bucket of the range, empty ones included (charts need the gaps).
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
                errors: 0,
            }),
        }
        t += step;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_text_is_literal() {
        assert_eq!(like_pattern("50%_off\\"), "%50\\%\\_off\\\\%");
    }

    #[test]
    fn about_sixty_whole_second_buckets() {
        assert_eq!(bucket_ms(0, 15 * 60 * 1000), 15_000);
        assert_eq!(bucket_ms(0, 3_600_000), 60_000);
        assert_eq!(bucket_ms(0, 10), 1000);
        let b = bucket_ms(0, 7 * 24 * 3_600_000);
        assert_eq!(b % 1000, 0);
        assert!(7 * 24 * 3_600_000 / b <= 61);
        assert_eq!(bucket_ms(0, 15 * 60 * 1000 + 40), 15_000);
    }

    #[test]
    fn empty_buckets_are_filled() {
        let got = fill(
            0,
            4000,
            1000,
            vec![Bucket {
                t: 2000,
                count: 3,
                errors: 1,
            }],
        );
        assert_eq!(got.len(), 4);
        assert_eq!(got[2].count, 3);
        assert_eq!(got[0].count + got[1].count + got[3].count, 0);
    }
}
