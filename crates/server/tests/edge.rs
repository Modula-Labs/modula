//! The edge: HTTP access logs (stored, searched, rolled up into request
//! charts) and per-service edge rules (staged, sent with routes).

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use serde_json::json;
use sqlx::PgPool;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn req(
    dep: &str,
    at_unix_ms: i64,
    method: &str,
    path: &str,
    status: u32,
    ms: u32,
) -> pb::HttpRequestLog {
    pb::HttpRequestLog {
        deployment_id: dep.into(),
        at_unix_ms,
        host: "api.example.com".into(),
        method: method.into(),
        path: path.into(),
        status,
        duration_us: ms * 1000,
        bytes: 512,
        client_ip: "198.51.100.7".into(),
        upstream: "10.88.0.2:8080".into(),
        user_agent: "curl/8\0".into(),
    }
}

fn batch(requests: Vec<pb::HttpRequestLog>) -> pb::HttpLogBatch {
    pb::HttpLogBatch {
        requests,
        dropped: 0,
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn http_requests_are_searchable_and_charted(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let dep = chain.deployment.to_string();
    let now = now_ms();
    modula_server::http_logs::record(
        app.db(),
        chain.node,
        &batch(vec![
            req(&dep, now - 50_000, "GET", "/a", 200, 8),
            req(&dep, now - 40_000, "POST", "/b", 404, 20),
            req(&dep, now - 30_000, "GET", "/API/x", 503, 900),
            // Unknown deployment: dropped.
            req(
                &Id::new().to_string(),
                now - 30_000,
                "GET",
                "/stray",
                200,
                1,
            ),
        ]),
    )
    .await
    .unwrap();
    // Another node can't write this deployment's requests.
    modula_server::http_logs::record(
        app.db(),
        Id::new(),
        &batch(vec![req(&dep, now - 20_000, "GET", "/forged", 200, 1)]),
    )
    .await
    .unwrap();

    let url = |q: &str| {
        format!(
            "/api/environments/{}/http-logs?since={}{q}",
            chain.environment,
            now - 15 * 60_000
        )
    };
    let r = app.call(Method::GET, &url(""), Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let reqs = r.json["requests"].as_array().unwrap();
    assert_eq!(reqs.len(), 3, "{}", r.json);
    assert_eq!(reqs[0]["path"], "/a");
    assert_eq!(reqs[0]["durationMs"], 8.0);
    assert_eq!(reqs[0]["serviceId"], chain.service.to_string());
    assert_eq!(reqs[0]["userAgent"], "curl/8");
    assert_eq!(reqs[2]["status"], 503);
    let hist = r.json["histogram"].as_array().unwrap();
    let sum = |k: &str| -> i64 { hist.iter().map(|b| b[k].as_i64().unwrap()).sum() };
    assert_eq!(
        (sum("count"), sum("status4xx"), sum("status5xx")),
        (3, 1, 1)
    );

    for (q, n) in [
        ("&status=5xx", 1),
        ("&status=2xx", 1),
        ("&method=POST", 1),
        ("&path=api", 1),
        ("&path=%25", 0),
        ("&status=4xx&method=GET", 0),
    ] {
        let r = app.call(Method::GET, &url(q), Some(&admin), None).await;
        assert_eq!(
            r.json["requests"].as_array().unwrap().len(),
            n,
            "{q}: {}",
            r.json
        );
    }
    let r = app
        .call(
            Method::GET,
            &url(&format!("&service={}", Id::new())),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["requests"].as_array().unwrap().len(), 0);
    for bad in ["&status=6xx", "&method=BREW"] {
        let r = app.call(Method::GET, &url(bad), Some(&admin), None).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let r = app
        .call(Method::GET, &url("&limit=1"), Some(&admin), None)
        .await;
    assert_eq!(r.json["truncated"], true);
    assert_eq!(r.json["requests"][0]["path"], "/API/x");

    // More requests in the same minutes add to the rollups.
    modula_server::http_logs::record(
        app.db(),
        chain.node,
        &batch(vec![req(&dep, now - 30_000, "GET", "/c", 200, 8)]),
    )
    .await
    .unwrap();
    let (requests, hist_total): (i64, i64) = sqlx::query_as(
        "SELECT sum(requests)::bigint, sum((SELECT sum(x) FROM unnest(latency_hist) AS x))::bigint
         FROM http_rollups",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((requests, hist_total), (4, 4));

    let r = app
        .call(
            Method::GET,
            &format!(
                "/api/service-instances/{}/http-metrics?range=1h",
                chain.instance
            ),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["stepSecs"], 60);
    let points = r.json["points"].as_array().unwrap();
    assert_eq!(points.len(), 60);
    let total: i64 = points.iter().map(|p| p["requests"].as_i64().unwrap()).sum();
    let s5: i64 = points
        .iter()
        .map(|p| p["status5xx"].as_i64().unwrap())
        .sum();
    assert_eq!((total, s5), (4, 1));
    assert!(points.iter().any(|p| p["p95Ms"].as_f64().is_some()));
    assert!(points.iter().any(|p| p["p95Ms"].is_null()));

    let r = app
        .call(
            Method::GET,
            &format!(
                "/api/projects/{}/http-metrics?environment={}&range=24h",
                chain.project, chain.environment
            ),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let total: i64 = r.json["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["requests"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 4);

    sqlx::query("UPDATE http_logs SET at = at - interval '8 days'")
        .execute(app.db())
        .await
        .unwrap();
    sqlx::query("UPDATE http_rollups SET minute = minute - interval '8 days'")
        .execute(app.db())
        .await
        .unwrap();
    let purged = modula_server::http_logs::purge(app.db()).await.unwrap();
    assert!(purged >= 5, "{purged}");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn edge_rules_are_staged_and_sent_with_routes(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let rules = json!([
        { "name": "Admin", "enabled": true,
          "when": [[{ "field": "path", "op": "prefix", "value": "/admin" }]],
          "then": { "type": "block", "status": 403 } },
        { "name": "Off", "enabled": false, "when": [],
          "then": { "type": "rateLimit", "requests": 10, "windowSecs": 60, "key": "ip" } }
    ]);
    let config = json!({
        "source": { "type": "image", "image": "nginx:alpine" }, "port": 80, "edgeRules": rules
    });
    let path = format!("/api/service-instances/{}", chain.instance);
    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({ "config": config })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["config"]["edgeRules"], rules);
    // Staged: a pending change until the next deploy.
    let r = app
        .call(
            Method::GET,
            &format!("/api/environments/{}/pending", chain.environment),
            Some(&admin),
            None,
        )
        .await;
    assert!(r.json.to_string().contains("edgeRules"), "{}", r.json);

    let mut bad = config.clone();
    bad["edgeRules"][0]["then"]["status"] = json!(200);
    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({ "config": bad })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.json.to_string().contains("edge rule 1"), "{}", r.json);

    // The live deployment's snapshot is what nodes get, on proxied domains too.
    sqlx::query("UPDATE deployments SET config = $2 WHERE id = $1")
        .bind(chain.deployment)
        .bind(&config)
        .execute(app.db())
        .await
        .unwrap();
    sqlx::query("UPDATE domains SET proxied = true WHERE id = $1")
        .bind(chain.domain)
        .execute(app.db())
        .await
        .unwrap();
    let ds = modula_server::deploy::desired::build(&app.state, chain.node)
        .await
        .unwrap();
    let route = &ds.routes[0];
    assert!(route.proxied);
    assert_eq!(route.rules.len(), 1, "disabled rules aren't sent");
    assert_eq!(
        route.rules[0].action,
        Some(pb::edge_rule::Action::Block(pb::EdgeBlock {
            status: 403,
            body: String::new()
        }))
    );

    // A node whose Caddy lacks a module says so; the service shows it.
    sqlx::query("UPDATE nodes SET edge_unsupported = '{rateLimit}' WHERE id = $1")
        .bind(chain.node)
        .execute(app.db())
        .await
        .unwrap();
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(
        r.json["edgeUnsupported"],
        json!(["rateLimit"]),
        "{}",
        r.json
    );
}
