//! Where a new service goes: off the main server, close to what it references.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use serde_json::json;
use sqlx::PgPool;

async fn node(app: &TestApp, name: &str, location: &str, main: bool) -> Id {
    let id = Id::new();
    let info = if main {
        json!({ "capabilities": ["dashboard"] })
    } else {
        json!({})
    };
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at, location, info)
         VALUES ($1, $2, 'online', $1::text, now() + interval '1 day', $3, $4)",
    )
    .bind(id)
    .bind(name)
    .bind(location)
    .bind(info)
    .execute(app.db())
    .await
    .unwrap();
    id
}

async fn instance(app: &TestApp, chain: &Chain, name: &str, node: Option<Id>) -> Id {
    let (service, id) = (Id::new(), Id::new());
    sqlx::query("INSERT INTO services (id, project_id, name) VALUES ($1, $2, $3)")
        .bind(service)
        .bind(chain.project)
        .bind(name)
        .execute(app.db())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO service_instances (id, service_id, environment_id, node_id, config)
         VALUES ($1, $2, $3, $4, '{}'::jsonb)",
    )
    .bind(id)
    .bind(service)
    .bind(chain.environment)
    .bind(node)
    .execute(app.db())
    .await
    .unwrap();
    id
}

async fn choose(app: &TestApp, instance: Id) -> Id {
    let mut conn = app.db().acquire().await.unwrap();
    modula_server::deploy::placement::choose(&mut conn, instance)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn new_services_go_to_a_worker_near_what_they_reference(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    // seed_chain's server becomes the main one, in Falkenstein, with web on it.
    sqlx::query(
        "UPDATE nodes SET status = 'online', location = 'hetzner:fsn1',
                info = '{\"capabilities\": [\"dashboard\"]}' WHERE id = $1",
    )
    .bind(chain.node)
    .execute(app.db())
    .await
    .unwrap();
    let us = node(&app, "us", "hetzner:ash", false).await;
    // Only the main server and one worker: the worker, though it's far.
    let a = instance(&app, &chain, "a", None).await;
    assert_eq!(choose(&app, a).await, us);

    // A database in Nuremberg, and a busier worker there.
    let de = node(&app, "de", "hetzner:nbg1", false).await;
    let db = instance(&app, &chain, "db", Some(de)).await;
    sqlx::query(
        "INSERT INTO deployments (id, service_instance_id, node_id, status, image, config)
         VALUES ($1, $2, $3, 'active', 'postgres', '{}'::jsonb)",
    )
    .bind(Id::new())
    .bind(db)
    .bind(de)
    .execute(app.db())
    .await
    .unwrap();
    // An app referencing it goes next to it.
    let api = instance(&app, &chain, "api", None).await;
    sqlx::query(
        "INSERT INTO variables (id, environment_id, service_instance_id, name, value_sealed, refs)
         VALUES ($1, $2, $3, 'DATABASE_URL', '\\x00', '{db.DATABASE_URL}')",
    )
    .bind(Id::new())
    .bind(chain.environment)
    .bind(api)
    .execute(app.db())
    .await
    .unwrap();
    assert_eq!(choose(&app, api).await, de);
    // Placed: sticks there.
    assert_eq!(choose(&app, api).await, de);

    // The dashboard asks the same question before there's an instance.
    let path = format!(
        "/api/environments/{}/placement-suggestion?references=db",
        chain.environment
    );
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    let servers = r.json["servers"].as_array().unwrap();
    assert_eq!(servers[0]["nodeId"], de.to_string());
    assert_eq!(servers[0]["recommended"], true);
    let last = servers.last().unwrap();
    assert_eq!(last["nodeId"], chain.node.to_string());
    assert_eq!(last["main"], true);

    // Another environment's instance is refused.
    let path = format!(
        "/api/environments/{}/placement-suggestion?instance={}",
        chain.environment,
        Id::new()
    );
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
