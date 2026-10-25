//! Uptime: the minute monitor records whether each service that should run
//! does, and the API buckets it for the strip.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use sqlx::PgPool;

async fn exec(db: &PgPool, sql: &'static str) {
    sqlx::query(sql).execute(db).await.unwrap();
}

async fn rows(db: &PgPool) -> Vec<(bool, Option<String>)> {
    sqlx::query_as("SELECT up, reason FROM uptime_minutes ORDER BY minute")
        .fetch_all(db)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn running_crashed_unreachable_and_sleeping_services(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let db = app.db();
    let record = || modula_server::metrics::record_uptime(db);

    // Active on an online server: up, once per minute.
    exec(
        db,
        "UPDATE nodes SET status = 'online', last_seen_at = now()",
    )
    .await;
    assert_eq!(record().await.unwrap(), 1);
    assert_eq!(record().await.unwrap(), 0);
    assert_eq!(rows(db).await, [(true, None)]);

    // No server seen at all (this server just restarted): nothing recorded.
    exec(db, "DELETE FROM uptime_minutes").await;
    exec(
        db,
        "UPDATE nodes SET last_seen_at = now() - interval '5 minutes'",
    )
    .await;
    assert_eq!(record().await.unwrap(), 0);

    // Its server went silent while another is seen: down, unreachable.
    exec(
        db,
        "INSERT INTO nodes (id, name, cert_serial, cert_expires_at, status, last_seen_at)
         VALUES ('00000000000other', 'o', 'other', now() + interval '1 day', 'online', now())",
    )
    .await;
    record().await.unwrap();
    assert_eq!(rows(db).await, [(false, Some("unreachable".into()))]);

    // Crashed: down, crashed.
    exec(db, "DELETE FROM uptime_minutes").await;
    exec(db, "UPDATE nodes SET last_seen_at = now()").await;
    exec(db, "UPDATE deployments SET status = 'crashed'").await;
    record().await.unwrap();
    assert_eq!(rows(db).await, [(false, Some("crashed".into()))]);

    // Sleeping (scale to zero), removed (stopped) or a cron job: no row.
    for sql in [
        "UPDATE deployments SET status = 'sleeping'",
        "UPDATE deployments SET status = 'removed'",
        "UPDATE deployments SET status = 'active', config = '{\"cronSchedule\": \"* * * * *\"}'",
    ] {
        exec(db, "DELETE FROM uptime_minutes").await;
        exec(db, sql).await;
        assert_eq!(record().await.unwrap(), 0, "{sql}");
    }

    // History: 30 minutes ago, 10 up then 2 down minutes; nothing before.
    exec(db, "DELETE FROM uptime_minutes").await;
    sqlx::query(
        "INSERT INTO uptime_minutes (service_instance_id, minute, up, reason)
         SELECT $1, date_trunc('minute', now()) - make_interval(mins => m), m > 22,
                CASE WHEN m > 22 THEN NULL ELSE 'crashed' END
         FROM generate_series(21, 32) m",
    )
    .bind(chain.instance)
    .execute(db)
    .await
    .unwrap();
    let r = app
        .call(
            Method::GET,
            &format!("/api/service-instances/{}/uptime?range=1h", chain.instance),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["range"], "1h");
    assert_eq!(r.json["stepSecs"], 60);
    let buckets = r.json["buckets"].as_array().unwrap();
    assert_eq!(buckets.len(), 60);
    let count = |s: &str| buckets.iter().filter(|b| b["state"] == s).count();
    assert_eq!((count("up"), count("down"), count("none")), (10, 2, 48));
    let down = buckets.iter().find(|b| b["state"] == "down").unwrap();
    assert_eq!(down["downMs"], 60_000);
    assert_eq!(down["reason"], "crashed");
    assert_eq!(buckets[59]["state"], "none");
    let ratio = r.json["uptime"].as_f64().unwrap();
    assert!((ratio - 10.0 / 12.0).abs() < 1e-9, "{ratio}");

    // The project's view (one environment) sees the same.
    let r = app
        .call(
            Method::GET,
            &format!(
                "/api/projects/{}/uptime?environment={}&range=24h",
                chain.project, chain.environment
            ),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["buckets"].as_array().unwrap().len(), 48);
    assert!((r.json["uptime"].as_f64().unwrap() - 10.0 / 12.0).abs() < 1e-9);

    // Kept as long as metric samples.
    exec(
        db,
        "UPDATE uptime_minutes SET minute = minute - interval '8 days'",
    )
    .await;
    assert_eq!(modula_server::metrics::purge_uptime(db).await.unwrap(), 12);

    let r = app
        .call(
            Method::GET,
            &format!("/api/service-instances/{}/uptime?range=2d", chain.instance),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
