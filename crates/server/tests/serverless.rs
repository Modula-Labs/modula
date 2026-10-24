//! Serverless services: the `sleeping` state is live (routed, desired,
//! retired like `active`), wakes record their cold start, and a volume
//! rules serverless out.

mod common;

use common::*;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use modula_server::deploy::{self, config::ServiceConfig, start};
use serde_json::json;
use sqlx::PgPool;

fn serverless_config() -> ServiceConfig {
    serde_json::from_value(json!({
        "source": { "type": "image", "image": "nginx:alpine" },
        "port": 80,
        "serverless": { "idleMinutes": 5 }
    }))
    .unwrap()
}

async fn deploy(app: &TestApp, env: Id, instance: Id) -> Result<start::Started, String> {
    let mut tx = app.db().begin().await.unwrap();
    let d = start::NewDeployment {
        instance,
        config: serverless_config(),
        trigger: start::Trigger::Manual,
        created_by: None,
        commit: None,
        local_git: None,
        prebuilt: None,
        source_deployment: None,
    };
    match start::start(&app.state, &mut tx, env, d).await {
        Ok(s) => {
            tx.commit().await.unwrap();
            Ok(s)
        }
        Err(e) => Err(format!("{e:?}")),
    }
}

fn event(dep: Id, phase: pb::DeploymentPhase, cold_start_ms: u32) -> pb::DeploymentEvent {
    pb::DeploymentEvent {
        deployment_id: dep.to_string(),
        phase: phase as i32,
        message: "x".into(),
        cold_start_ms,
        ..Default::default()
    }
}

async fn row(app: &TestApp, dep: Id) -> (String, Option<i32>, bool) {
    sqlx::query_as(
        "SELECT status, cold_start_ms, woke_at IS NOT NULL FROM deployments WHERE id = $1",
    )
    .bind(dep)
    .fetch_one(app.db())
    .await
    .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn sleeping_is_live_and_wakes_record_the_cold_start(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let (env, instance, node) = (chain.environment, chain.instance, chain.node);

    // seed_chain's variable is placeholder ciphertext, its deployment has no config.
    for q in [
        "DELETE FROM variables",
        "UPDATE deployments SET config = i.config FROM service_instances i WHERE i.id = service_instance_id",
    ] {
        sqlx::query(q).execute(app.db()).await.unwrap();
    }
    // A volume rules it out.
    let err = deploy(&app, env, instance).await.unwrap_err();
    assert!(err.contains("can't be serverless"), "{err}");
    sqlx::query("DELETE FROM volumes")
        .execute(app.db())
        .await
        .unwrap();

    let first = deploy(&app, env, instance).await.unwrap();
    let ds = deploy::desired::build(&app.state, node).await.unwrap();
    let w = ds
        .workloads
        .iter()
        .find(|w| w.deployment_id == first.id.to_string())
        .unwrap();
    assert_eq!(w.serverless, Some(pb::Serverless { idle_secs: 300 }));
    use pb::DeploymentPhase::{Active, Crashed, Sleeping};
    deploy::state::apply_event(app.db(), node, &event(first.id, Active, 0))
        .await
        .unwrap();
    assert_eq!(row(&app, first.id).await, ("active".into(), None, false));

    // Asleep: still desired and routed, and the service reads as sleeping.
    deploy::state::apply_event(app.db(), node, &event(first.id, Sleeping, 0))
        .await
        .unwrap();
    assert_eq!(row(&app, first.id).await.0, "sleeping");
    let ds = deploy::desired::build(&app.state, node).await.unwrap();
    let dep = first.id.to_string();
    assert!(ds.workloads.iter().any(|w| w.deployment_id == dep));
    assert!(ds.routes.iter().any(|r| r.deployment_id == dep));
    let loaded = modula_server::api::services::load_instance(&app.state, instance)
        .await
        .unwrap();
    assert_eq!(loaded.status, "sleeping");
    // Replayed after a reconnect: nothing changes.
    deploy::state::apply_event(app.db(), node, &event(first.id, Sleeping, 0))
        .await
        .unwrap();
    assert_eq!(row(&app, first.id).await.0, "sleeping");

    // Woken: active, with the cold start.
    deploy::state::apply_event(app.db(), node, &event(first.id, Active, 420))
        .await
        .unwrap();
    assert_eq!(
        row(&app, first.id).await,
        ("active".into(), Some(420), true)
    );
    // A replayed ACTIVE keeps it.
    deploy::state::apply_event(app.db(), node, &event(first.id, Active, 0))
        .await
        .unwrap();
    assert_eq!(
        row(&app, first.id).await,
        ("active".into(), Some(420), true)
    );

    // A new deployment retires a sleeping one like an active one.
    deploy::state::apply_event(app.db(), node, &event(first.id, Sleeping, 0))
        .await
        .unwrap();
    let second = deploy(&app, env, instance).await.unwrap();
    deploy::state::apply_event(app.db(), node, &event(second.id, Active, 0))
        .await
        .unwrap();
    assert_eq!(row(&app, first.id).await.0, "removed");
    assert_eq!(row(&app, second.id).await.0, "active");

    // A crash while asleep (it can't serve) is a crash.
    deploy::state::apply_event(app.db(), node, &event(second.id, Sleeping, 0))
        .await
        .unwrap();
    deploy::state::apply_event(app.db(), node, &event(second.id, Crashed, 0))
        .await
        .unwrap();
    assert_eq!(row(&app, second.id).await.0, "crashed");
}
