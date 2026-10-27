//! Moving a service to another server (the region picker).

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use serde_json::json;
use sqlx::PgPool;

async fn node(app: &TestApp, workspace: Option<Id>, status: &str) -> Id {
    let id = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, workspace_id, name, status, cert_serial, cert_expires_at)
         VALUES ($1, $2, 'worker', $3, $1::text, now() + interval '1 day')",
    )
    .bind(id)
    .bind(workspace)
    .bind(status)
    .execute(app.db())
    .await
    .unwrap();
    id
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_service_moves_and_redeploys_on_the_new_server(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    // seed_chain's variable holds placeholder ciphertext a deploy can't open.
    sqlx::query("DELETE FROM variables")
        .execute(app.db())
        .await
        .unwrap();
    // seed_chain gives the service a volume; this one has none.
    sqlx::query("DELETE FROM volumes WHERE service_instance_id = $1")
        .bind(chain.instance)
        .execute(app.db())
        .await
        .unwrap();
    let path = format!("/api/service-instances/{}/server", chain.instance);

    let other_ws = app.create_workspace(&admin, "other").await;
    let foreign = node(&app, Some(other_ws), "online").await;
    let revoked = node(&app, None, "revoked").await;
    for bad in [foreign, revoked, Id::new()] {
        let r = app
            .call(
                Method::PATCH,
                &path,
                Some(&admin),
                Some(json!({ "nodeId": bad })),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
    }

    let target = node(&app, Some(ws), "online").await;
    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({ "nodeId": target })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["nodeId"], target.to_string());
    let dep: Id = r.json["deploymentId"].as_str().unwrap().parse().unwrap();
    let (on, inst_node): (Option<Id>, Option<Id>) = sqlx::query_as(
        "SELECT d.node_id, i.node_id FROM deployments d JOIN service_instances i ON i.id = d.service_instance_id
         WHERE d.id = $1",
    )
    .bind(dep)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(on, Some(target));
    assert_eq!(inst_node, Some(target));

    // Again: already there, nothing happens.
    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({ "nodeId": target })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json["deploymentId"].is_null());

    // A volume's data can't follow.
    sqlx::query(
        "INSERT INTO volumes (id, service_instance_id, mount_path) VALUES ($1, $2, '/data')",
    )
    .bind(Id::new())
    .bind(chain.instance)
    .execute(app.db())
    .await
    .unwrap();
    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({ "nodeId": chain.node })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{}", r.json);
}
