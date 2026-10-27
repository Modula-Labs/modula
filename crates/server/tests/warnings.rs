//! The project page's warnings: failed deployments and apps on the main server.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use serde_json::Value;
use sqlx::PgPool;

fn kinds(items: &Value) -> Vec<&str> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["kind"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn warnings_list_failed_deploys_and_apps_on_the_main_server(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let path = format!("/api/environments/{}/warnings", chain.environment);
    sqlx::query("UPDATE nodes SET status = 'online' WHERE id = $1")
        .bind(chain.node)
        .execute(app.db())
        .await
        .unwrap();

    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(kinds(&r.json["items"]).is_empty(), "{}", r.json);

    // The latest deployment failed, and the server is the main one.
    sqlx::query("UPDATE deployments SET status = 'failed', error = 'build failed' WHERE id = $1")
        .bind(chain.deployment)
        .execute(app.db())
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET info = '{\"capabilities\": [\"dashboard\"]}' WHERE id = $1")
        .bind(chain.node)
        .execute(app.db())
        .await
        .unwrap();
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    let items = &r.json["items"];
    assert_eq!(kinds(items), ["deploy_failed", "main_server"], "{}", r.json);
    assert_eq!(items[0]["severity"], "error");
    assert_eq!(items[0]["serviceId"], chain.service.to_string());
    assert_eq!(items[0]["action"]["type"], "openDeployment");
    assert_eq!(
        items[0]["action"]["deploymentId"],
        chain.deployment.to_string()
    );
    assert!(
        items[0]["docsUrl"]
            .as_str()
            .unwrap()
            .ends_with("/deploy/deployments/#troubleshooting")
    );
    // No other server yet: a note to add one.
    assert_eq!(items[1]["severity"], "info");
    assert_eq!(items[1]["action"]["type"], "addServer");

    // With a worker: move this service there.
    let worker = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at)
         VALUES ($1, 'worker', 'online', $1::text, now() + interval '1 day')",
    )
    .bind(worker)
    .execute(app.db())
    .await
    .unwrap();
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    let items = &r.json["items"];
    assert_eq!(kinds(items), ["deploy_failed", "main_server"], "{}", r.json);
    assert_eq!(items[1]["severity"], "warning");
    assert_eq!(items[1]["serviceInstanceId"], chain.instance.to_string());
    assert_eq!(items[1]["action"]["type"], "moveServer");
    assert_eq!(items[1]["action"]["nodeId"], worker.to_string());
    assert!(
        items[1]["docsUrl"]
            .as_str()
            .unwrap()
            .ends_with("/deploy/servers/#keep-apps-off-the-main-server")
    );

    // Moved, and deployed fine: nothing left.
    sqlx::query("UPDATE service_instances SET node_id = $2 WHERE id = $1")
        .bind(chain.instance)
        .bind(worker)
        .execute(app.db())
        .await
        .unwrap();
    sqlx::query("UPDATE deployments SET status = 'active' WHERE id = $1")
        .bind(chain.deployment)
        .execute(app.db())
        .await
        .unwrap();
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert!(kinds(&r.json["items"]).is_empty(), "{}", r.json);

    // A reference to a service that doesn't exist.
    sqlx::query("UPDATE variables SET refs = '{gone.URL}' WHERE id = $1")
        .bind(chain.variable)
        .execute(app.db())
        .await
        .unwrap();
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(kinds(&r.json["items"]), ["missing_reference"], "{}", r.json);
}
