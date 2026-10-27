//! Project observability: stored logs searched per environment, project-wide
//! metrics, and webhooks.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde_json::json;
use sqlx::PgPool;

fn line(deployment: &str, at_unix_ms: i64, stderr: bool, text: &str) -> pb::ShippedLine {
    pb::ShippedLine {
        deployment_id: deployment.into(),
        at_unix_ms,
        stderr,
        text: text.into(),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn shipped_logs_are_searchable_per_environment(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let dep = chain.deployment.to_string();
    let now = now_ms();
    let batch = pb::LogBatch {
        lines: vec![
            line(&dep, now - 50_000, false, "listening on :8080"),
            line(&dep, now - 40_000, true, "error: 100% of_disk used\0"),
            line(&dep, now - 30_000, false, "GET /health 200"),
            // Unknown deployment: dropped.
            line(&Id::new().to_string(), now - 30_000, false, "stray"),
        ],
        dropped: 0,
    };
    modula_server::logs::record(&app.state, chain.node, &batch)
        .await
        .unwrap();
    // Another node can't write this deployment's logs.
    modula_server::logs::record(
        &app.state,
        Id::new(),
        &pb::LogBatch {
            lines: vec![line(&dep, now - 20_000, false, "forged")],
            dropped: 0,
        },
    )
    .await
    .unwrap();

    let url = |q: &str| {
        format!(
            "/api/environments/{}/logs?since={}{q}",
            chain.environment,
            now - 15 * 60_000
        )
    };
    let r = app.call(Method::GET, &url(""), Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let lines = r.json["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3, "{}", r.json);
    assert_eq!(lines[0]["line"], "listening on :8080");
    assert_eq!(lines[1]["stream"], "stderr");
    assert_eq!(lines[1]["line"], "error: 100% of_disk used");
    assert_eq!(lines[2]["serviceId"], chain.service.to_string());
    assert_eq!(r.json["truncated"], false);
    assert_eq!(r.json["bucketMs"], 15_000);
    let hist = r.json["histogram"].as_array().unwrap();
    assert!((59..=61).contains(&hist.len()), "{}", hist.len());
    let total: i64 = hist.iter().map(|b| b["count"].as_i64().unwrap()).sum();
    let errors: i64 = hist.iter().map(|b| b["errors"].as_i64().unwrap()).sum();
    assert_eq!((total, errors), (3, 1));

    // Search is literal and case-insensitive; `%` and `_` aren't wildcards.
    let r = app
        .call(Method::GET, &url("&q=100%25%20OF_"), Some(&admin), None)
        .await;
    assert_eq!(r.json["lines"].as_array().unwrap().len(), 1, "{}", r.json);
    let r = app
        .call(Method::GET, &url("&q=_"), Some(&admin), None)
        .await;
    assert_eq!(r.json["lines"].as_array().unwrap().len(), 1, "{}", r.json);
    let r = app
        .call(
            Method::GET,
            &url(&format!("&service={}", Id::new())),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["lines"].as_array().unwrap().len(), 0);
    // One deployment's lines: the histogram is filtered too.
    let r = app
        .call(
            Method::GET,
            &url(&format!("&deployment={dep}")),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["lines"].as_array().unwrap().len(), 3, "{}", r.json);
    let r = app
        .call(
            Method::GET,
            &url(&format!("&deployment={}", Id::new())),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["lines"].as_array().unwrap().len(), 0);
    let empty: i64 = r.json["histogram"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["count"].as_i64().unwrap())
        .sum();
    assert_eq!(empty, 0);

    // The newest lines win when truncated.
    let r = app
        .call(Method::GET, &url("&limit=2"), Some(&admin), None)
        .await;
    assert_eq!(r.json["truncated"], true);
    assert_eq!(r.json["lines"][1]["line"], "GET /health 200");

    let r = app
        .call(
            Method::GET,
            &format!(
                "/api/environments/{}/logs?since={}&until={}",
                chain.environment,
                now,
                now - 1
            ),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    sqlx::query("UPDATE log_lines SET at = at - interval '8 days'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(modula_server::logs::purge(app.db()).await.unwrap(), 3);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn project_metrics_sum_the_environment(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let now = now_ms();
    let minute = now - now % 60_000 - 60_000;
    let report = pb::MetricsReport {
        workloads: vec![pb::WorkloadMetrics {
            deployment_id: chain.deployment.to_string(),
            at_unix_ms: minute + 1_000,
            cpu_percent: 12.0,
            memory_bytes: 1000,
            net_rx_bytes_per_sec: 1.0,
            net_tx_bytes_per_sec: 2.0,
        }],
        host: None,
    };
    modula_server::metrics::record(app.db(), chain.node, &report)
        .await
        .unwrap();
    let r = app
        .call(
            Method::GET,
            &format!(
                "/api/projects/{}/metrics?environment={}&range=1h",
                chain.project, chain.environment
            ),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["points"][0]["cpuPercent"], 12.0);
    // Another project's environment adds nothing.
    let r = app
        .call(
            Method::GET,
            &format!(
                "/api/projects/{}/metrics?environment={}&range=1h",
                chain.project,
                Id::new()
            ),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["points"].as_array().unwrap().len(), 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn webhooks_are_created_listed_and_removed(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let base = format!("/api/projects/{}/webhooks", chain.project);

    for (body, why) in [
        (
            json!({"url": "http://169.254.169.254/latest", "events": ["deployment.failed"]}),
            "metadata",
        ),
        (
            json!({"url": "https://example.com/h", "events": []}),
            "no events",
        ),
        (
            json!({"url": "https://example.com/h", "events": ["nope"]}),
            "unknown event",
        ),
    ] {
        let r = app
            .call(Method::POST, &base, Some(&admin), Some(body))
            .await;
        assert!(r.status.is_client_error(), "{why}: {}", r.json);
    }
    let r = app
        .call(
            Method::POST,
            &base,
            Some(&admin),
            Some(json!({"url": "https://example.com/hook?token=abc", "events": ["deployment.failed", "deployment.succeeded", "deployment.failed"]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let secret = r.json["secret"].as_str().unwrap();
    assert!(secret.starts_with("whsec_"));
    let id = r.json["id"].as_str().unwrap().to_owned();
    assert_eq!(
        r.json["events"],
        json!(["deployment.failed", "deployment.succeeded"])
    );

    let r = app.call(Method::GET, &base, Some(&admin), None).await;
    let list = r.json.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].get("secret").is_none());
    // Sealed at rest.
    let stored: Vec<u8> = sqlx::query_scalar("SELECT secret_sealed FROM webhooks")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert!(!stored.windows(secret.len()).any(|w| w == secret.as_bytes()));
    // The audit log keeps the host, never the URL's token.
    let meta: serde_json::Value =
        sqlx::query_scalar("SELECT metadata FROM audit_log WHERE action = 'webhook.create'")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(!meta.to_string().contains("abc"), "{meta}");

    let r = app
        .call(
            Method::DELETE,
            &format!("/api/webhooks/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = app.call(Method::GET, &base, Some(&admin), None).await;
    assert_eq!(r.json.as_array().unwrap().len(), 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn stored_logs_mask_secret_values(pool: PgPool) {
    use modula_server::deploy::env;
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let vars: env::EnvMap = [(
        "API_TOKEN".to_owned(),
        secrecy::SecretString::from("tok_9f8e7d6c5b4a".to_owned()),
    )]
    .into();
    let sealed = env::seal_snapshot(
        &app.state.keys,
        app.db(),
        chain.environment,
        chain.deployment,
        &vars,
    )
    .await
    .unwrap();
    sqlx::query("UPDATE deployments SET env_sealed = $1 WHERE id = $2")
        .bind(sealed)
        .bind(chain.deployment)
        .execute(app.db())
        .await
        .unwrap();
    let batch = pb::LogBatch {
        lines: vec![line(
            &chain.deployment.to_string(),
            now_ms(),
            false,
            "calling with tok_9f8e7d6c5b4a",
        )],
        dropped: 0,
    };
    modula_server::logs::record(&app.state, chain.node, &batch)
        .await
        .unwrap();
    let stored: String = sqlx::query_scalar("SELECT line FROM log_lines")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(stored, format!("calling with {}", env::Redactor::MASK));
}
