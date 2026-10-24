//! Metrics: nodes report samples, the API returns them bucketed.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use sqlx::PgPool;

fn sample(deployment: &str, at_unix_ms: i64, cpu: f64, mem: u64) -> pb::WorkloadMetrics {
    pb::WorkloadMetrics {
        deployment_id: deployment.into(),
        at_unix_ms,
        cpu_percent: cpu,
        memory_bytes: mem,
        net_rx_bytes_per_sec: 100.0,
        net_tx_bytes_per_sec: f64::NAN,
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn samples_are_bucketed_and_only_the_owning_node_counts(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let dep = chain.deployment.to_string();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // Same minute: averaged into one point.
    let minute = now - now % 60_000 - 60_000;
    let report = pb::MetricsReport {
        workloads: vec![
            sample(&dep, minute + 1_000, 10.0, 100),
            sample(&dep, minute + 31_000, 30.0, 300),
            sample(&Id::new().to_string(), minute + 1_000, 99.0, 999),
        ],
    };
    modula_server::metrics::record(app.db(), chain.node, &report)
        .await
        .unwrap();
    // Another node can't write this deployment's numbers.
    modula_server::metrics::record(
        app.db(),
        Id::new(),
        &pb::MetricsReport {
            workloads: vec![sample(&dep, minute + 2_000, 500.0, 5)],
        },
    )
    .await
    .unwrap();

    let r = app
        .call(
            Method::GET,
            &format!("/api/service-instances/{}/metrics?range=1h", chain.instance),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["range"], "1h");
    assert_eq!(r.json["stepSecs"], 60);
    let points = r.json["points"].as_array().unwrap();
    assert_eq!(points.len(), 1, "{}", r.json);
    assert_eq!(points[0]["t"], minute);
    assert_eq!(points[0]["cpuPercent"], 20.0);
    assert_eq!(points[0]["memoryBytes"], 200);
    assert_eq!(points[0]["netRxBytesPerSec"], 100.0);
    // A non-finite value from a node is stored as 0.
    assert_eq!(points[0]["netTxBytesPerSec"], 0.0);

    // Older than a week: purged.
    sqlx::query("UPDATE metric_samples SET at = at - interval '8 days'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(modula_server::metrics::purge(app.db()).await.unwrap(), 2);

    let r = app
        .call(
            Method::GET,
            &format!(
                "/api/service-instances/{}/metrics?range=nope",
                chain.instance
            ),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
