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
        host: None,
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
            host: None,
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

fn host(at_unix_ms: i64, cpu: f64, used: u64, load1: f64) -> pb::HostStats {
    pb::HostStats {
        at_unix_ms,
        cpu_percent: cpu,
        memory_used_bytes: used,
        memory_total_bytes: 4096,
        load1,
        net_rx_bytes_per_sec: 1000.0,
        net_tx_bytes_per_sec: f64::INFINITY,
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn server_usage_is_bucketed_with_its_disks(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let minute = now - now % 60_000 - 60_000;
    // No workloads: the server's own numbers are still kept.
    for h in [
        host(minute + 1_000, 20.0, 1000, 0.5),
        host(minute + 31_000, 40.0, 2000, 1.5),
    ] {
        modula_server::metrics::record(
            app.db(),
            chain.node,
            &pb::MetricsReport {
                workloads: vec![],
                host: Some(h),
            },
        )
        .await
        .unwrap();
    }
    modula_server::metrics::record_disk(
        app.db(),
        chain.node,
        &pb::DiskReport {
            filesystems: vec![pb::FilesystemUsage {
                path: "/var/lib/modula-node".into(),
                total_bytes: 1000,
                free_bytes: 400,
            }],
            volumes: vec![],
        },
    )
    .await
    .unwrap();

    let r = app
        .call(
            Method::GET,
            &format!("/api/nodes/{}/metrics?range=1h", chain.node),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["stepSecs"], 60);
    let points = r.json["points"].as_array().unwrap();
    assert_eq!(points.len(), 1, "{}", r.json);
    assert_eq!(points[0]["t"], minute);
    assert_eq!(points[0]["cpuPercent"], 30.0);
    assert_eq!(points[0]["memoryUsedBytes"], 1500);
    assert_eq!(points[0]["memoryTotalBytes"], 4096);
    assert_eq!(points[0]["load1"], 1.0);
    assert_eq!(points[0]["netRxBytesPerSec"], 1000.0);
    assert_eq!(points[0]["netTxBytesPerSec"], 0.0);
    assert_eq!(r.json["diskStepSecs"], 300);
    let disk = r.json["disk"].as_array().unwrap();
    assert_eq!(disk.len(), 1, "{}", r.json);
    assert_eq!(disk[0]["path"], "/var/lib/modula-node");
    assert_eq!(disk[0]["points"][0]["usedBytes"], 600);
    assert_eq!(disk[0]["points"][0]["totalBytes"], 1000);

    // The list carries the public IP (none reported here).
    let r = app
        .call(Method::GET, "/api/nodes", Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(r.json[0].get("publicIp").is_some(), "{}", r.json);

    // Older than a week: purged with the disk samples.
    sqlx::query("UPDATE node_samples SET at = at - interval '8 days'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(modula_server::alerts::purge(app.db()).await.unwrap(), 2);
}
