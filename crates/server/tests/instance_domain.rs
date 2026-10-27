//! The dashboard's domain and the apps domain, set after an IP-only install.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::*;
use modula_server::domains::dns::{RecordKind, StaticDns};
use serde_json::json;
use sqlx::PgPool;

const IP: &str = "203.0.113.10";

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_domain_is_added_after_an_ip_install(pool: PgPool) {
    let mut cfg = config();
    cfg.public_url = format!("https://{IP}").parse().unwrap();
    cfg.apps_domain = None;
    let app = TestApp::with_config(pool, cfg);
    let dns = Arc::new(StaticDns::default());
    app.state.set_dns_resolver(dns.clone());
    let origin = format!("https://{IP}");
    assert_eq!(app.origin, origin);
    let admin = app.setup_admin().await;

    let r = app
        .call_at(
            &origin,
            Method::GET,
            "/api/instance/domain",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["installUrl"], format!("https://{IP}"));
    assert_eq!(r.json["serverIp"], IP);
    assert_eq!(r.json["dashboard"], serde_json::Value::Null);

    // No apps domain yet: generated domains explain where to add one.
    let chain = app
        .seed_chain(app.create_workspace(&admin, "ws").await)
        .await;
    let r = app
        .call_at(
            &origin,
            Method::POST,
            &format!("/api/service-instances/{}/domains", chain.instance),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.json);
    assert!(r.json.to_string().contains("Settings"), "{}", r.json);

    // DNS must point here first.
    let body = json!({ "dashboard": "deploy.example.com", "appsDomain": null });
    let r = app
        .call_at(
            &origin,
            Method::PUT,
            "/api/instance/domain",
            Some(&admin),
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.json);
    assert!(r.json.to_string().contains(IP), "{}", r.json);
    dns.set(RecordKind::A, "deploy.example.com", &["198.51.100.1"]);
    let r = app
        .call_at(
            &origin,
            Method::PUT,
            "/api/instance/domain",
            Some(&admin),
            Some(body.clone()),
        )
        .await;
    assert!(r.json.to_string().contains("198.51.100.1"), "{}", r.json);
    dns.set(RecordKind::A, "deploy.example.com", &[IP]);
    let r = app
        .call_at(
            &origin,
            Method::PUT,
            "/api/instance/domain",
            Some(&admin),
            Some(body),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["dashboard"], "deploy.example.com");
    assert_eq!(app.state.public_origin(), "https://deploy.example.com");

    // Both origins work: the domain, and the IP (the CLI, mid-switch).
    let r = app
        .call_at(
            "https://deploy.example.com",
            Method::POST,
            "/api/workspaces",
            Some(&admin),
            Some(json!({"name": "two"})),
        )
        .await;
    assert_ne!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    let r = app
        .call_at(
            &origin,
            Method::POST,
            "/api/workspaces",
            Some(&admin),
            Some(json!({"name": "three"})),
        )
        .await;
    assert_ne!(r.status, StatusCode::FORBIDDEN, "{}", r.json);
    let r = app
        .call_at(
            "https://evil.example.com",
            Method::POST,
            "/api/workspaces",
            Some(&admin),
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    // Custom domains can't take the dashboard's name.
    let r = app
        .call_at(
            &origin,
            Method::POST,
            &format!("/api/service-instances/{}/custom-domains", chain.instance),
            Some(&admin),
            Some(json!({"hostname": "deploy.example.com"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);

    // An apps domain turns generated domains on (proxied wildcard: skip the check).
    let r = app
        .call_at(&origin, Method::PUT, "/api/instance/domain", Some(&admin), Some(json!({
            "dashboard": "deploy.example.com", "appsDomain": "apps.example.com", "skipDnsCheck": true
        })))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let r = app
        .call_at(
            &origin,
            Method::POST,
            &format!("/api/service-instances/{}/domains", chain.instance),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(
        r.json["hostname"]
            .as_str()
            .unwrap()
            .ends_with(".apps.example.com"),
        "{}",
        r.json
    );

    // Saved: a restart keeps them.
    let (d, a): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT dashboard_domain, apps_domain FROM instance")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(
        (d.as_deref(), a.as_deref()),
        (Some("deploy.example.com"), Some("apps.example.com"))
    );

    // Removing the dashboard domain: back to the IP.
    let r = app
        .call_at(
            &origin,
            Method::PUT,
            "/api/instance/domain",
            Some(&admin),
            Some(
                json!({ "dashboard": "", "appsDomain": "apps.example.com", "skipDnsCheck": true }),
            ),
        )
        .await;
    assert_eq!(r.json["dashboard"], serde_json::Value::Null);
    assert_eq!(app.state.public_origin(), origin);
}
