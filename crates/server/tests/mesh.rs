//! Private networking: /24 allocation, peers, replica placement across
//! servers, the replica state machine and what each node is told.

mod common;

use common::*;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use modula_server::deploy::{self, config::ServiceConfig, start};
use serde_json::json;
use sqlx::PgPool;

const KEYS: [&str; 3] = [
    "YAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=",
    "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=",
    "TrMvSoP4jYQlY6RIzBgbssQqY3vxI2Pi+y71lOWWXX0=",
];

async fn node(app: &TestApp, name: &str) -> Id {
    let id = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at, public_ip)
         VALUES ($1, $2, 'online', $1::text, now() + interval '1 day', '203.0.113.' || (10 + (SELECT count(*) FROM nodes)))",
    )
    .bind(id)
    .bind(name)
    .execute(app.db())
    .await
    .unwrap();
    id
}

fn hello(key: &str, addr: &str) -> pb::Hello {
    pb::Hello {
        capabilities: vec!["mesh".into()],
        info: Some(pb::NodeInfo {
            wg_public_key: key.into(),
            wg_port: 51820,
            addresses: vec![addr.into()],
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn join_mesh(app: &TestApp, node: Id, key: &str, addr: &str) {
    let changed = modula_server::mesh::record_hello(app.db(), node, &hello(key, addr))
        .await
        .unwrap();
    assert!(changed);
}

async fn index(app: &TestApp, node: Id) -> Option<i16> {
    sqlx::query_scalar("SELECT mesh_index FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(app.db())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn nodes_get_a_subnet_and_each_other_as_peers(pool: PgPool) {
    let app = TestApp::new(pool);
    let (a, b) = (node(&app, "a").await, node(&app, "b").await);
    join_mesh(&app, a, KEYS[0], "10.0.0.2/16").await;
    join_mesh(&app, b, KEYS[1], "10.0.0.3/16").await;
    assert_eq!(index(&app, a).await, Some(1));
    assert_eq!(index(&app, b).await, Some(2));
    // The same Hello again changes nothing for the peers.
    let again = modula_server::mesh::record_hello(app.db(), a, &hello(KEYS[0], "10.0.0.2/16"))
        .await
        .unwrap();
    assert!(!again);
    // Without the capability (an older modula-node), nothing at all.
    let old = pb::Hello::default();
    assert!(
        !modula_server::mesh::record_hello(app.db(), a, &old)
            .await
            .unwrap()
    );

    let mesh = modula_server::mesh::build(app.db(), a)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(mesh.subnet, "10.80.1.0/24");
    assert_eq!(mesh.peers.len(), 1);
    let p = &mesh.peers[0];
    assert_eq!(p.public_key, KEYS[1]);
    assert_eq!(p.subnet, "10.80.2.0/24");
    assert_eq!(
        (p.endpoint.as_str(), p.private_path),
        ("10.0.0.3:51820", true)
    );

    // A revoked node leaves the mesh and its /24 is reused.
    let mut tx = app.db().begin().await.unwrap();
    modula_server::nodes::revoke(&mut tx, b).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(index(&app, b).await, None);
    let mesh = modula_server::mesh::build(app.db(), a)
        .await
        .unwrap()
        .unwrap();
    assert!(mesh.peers.is_empty());
    let c = node(&app, "c").await;
    join_mesh(&app, c, KEYS[2], "192.168.1.5/24").await;
    assert_eq!(index(&app, c).await, Some(2));
    // No shared private network: the public address.
    let mesh = modula_server::mesh::build(app.db(), a)
        .await
        .unwrap()
        .unwrap();
    assert!(!mesh.peers[0].private_path);
    assert!(
        mesh.peers[0].endpoint.starts_with("203.0.113."),
        "{:?}",
        mesh.peers[0]
    );
}

async fn deploy(
    app: &TestApp,
    env: Id,
    instance: Id,
    replicas: u32,
) -> Result<start::Started, String> {
    let config: ServiceConfig = serde_json::from_value(json!({
        "source": { "type": "image", "image": "nginx:alpine" }, "port": 80, "replicas": replicas
    }))
    .unwrap();
    let mut tx = app.db().begin().await.unwrap();
    let d = start::NewDeployment {
        instance,
        config,
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

fn event(dep: Id, phase: pb::DeploymentPhase) -> pb::DeploymentEvent {
    pb::DeploymentEvent {
        deployment_id: dep.to_string(),
        phase: phase as i32,
        message: "x".into(),
        ..Default::default()
    }
}

async fn status(app: &TestApp, dep: Id) -> String {
    sqlx::query_scalar("SELECT status FROM deployments WHERE id = $1")
        .bind(dep)
        .fetch_one(app.db())
        .await
        .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn replicas_run_one_per_server_and_balance_over_the_mesh(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    // seed_chain's variable is placeholder ciphertext; its volume would pin
    // the service to one replica.
    for q in [
        "DELETE FROM variables",
        "DELETE FROM volumes",
        "UPDATE deployments SET config = i.config FROM service_instances i WHERE i.id = service_instance_id",
    ] {
        sqlx::query(q).execute(app.db()).await.unwrap();
    }
    let a = chain.node;
    join_mesh(&app, a, KEYS[0], "10.0.0.2/16").await;

    // One server: two replicas don't fit.
    let err = deploy(&app, chain.environment, chain.instance, 2)
        .await
        .unwrap_err();
    assert!(err.contains("2 replicas need 2 online servers"), "{err}");

    let b = node(&app, "b").await;
    join_mesh(&app, b, KEYS[1], "10.0.0.3/16").await;
    let started = deploy(&app, chain.environment, chain.instance, 2)
        .await
        .unwrap();
    assert_eq!(started.node, a);
    assert!(started.kick.contains(&b));
    let replicas: Vec<(i16, Id, String)> = sqlx::query_as(
        "SELECT ordinal, node_id, mesh_ip FROM deployment_replicas WHERE deployment_id = $1 ORDER BY ordinal",
    )
    .bind(started.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        replicas,
        [
            (0, a, "10.80.1.2".to_owned()),
            (1, b, "10.80.2.2".to_owned())
        ]
    );

    // Both nodes run their replica, with its mesh address.
    for (n, ip) in [(a, "10.80.1.2"), (b, "10.80.2.2")] {
        let ds = deploy::desired::build(&app.state, n).await.unwrap();
        let w = ds
            .workloads
            .iter()
            .find(|w| w.deployment_id == started.id.to_string());
        assert_eq!(w.map(|w| w.mesh_ip.as_str()), Some(ip), "{n}");
    }

    // Live only once every replica is.
    let active = pb::DeploymentPhase::Active;
    deploy::state::apply_event(app.db(), a, &event(started.id, active))
        .await
        .unwrap();
    assert_eq!(status(&app, started.id).await, "deploying");
    // A node that runs no replica of it can't report on it.
    let stranger = node(&app, "c").await;
    deploy::state::apply_event(app.db(), stranger, &event(started.id, active))
        .await
        .unwrap();
    assert_eq!(status(&app, started.id).await, "deploying");
    let kick = deploy::state::apply_event(app.db(), b, &event(started.id, active))
        .await
        .unwrap();
    assert_eq!(status(&app, started.id).await, "active");
    assert!(kick.contains(&a) && kick.contains(&b));
    assert_eq!(
        status(&app, chain.deployment).await,
        "removed",
        "superseded"
    );

    // Each ingress node balances over its own replica and the other one.
    let ds = deploy::desired::build(&app.state, a).await.unwrap();
    let route = ds
        .routes
        .iter()
        .find(|r| r.deployment_id == started.id.to_string())
        .unwrap();
    assert_eq!(route.remote_upstreams, ["10.80.2.2:80"]);
    let ds = deploy::desired::build(&app.state, b).await.unwrap();
    let route = ds
        .routes
        .iter()
        .find(|r| r.deployment_id == started.id.to_string())
        .unwrap();
    assert_eq!(route.remote_upstreams, ["10.80.1.2:80"]);
    // The address book: both replicas, healthy.
    let mesh = ds.mesh.unwrap();
    let env = &mesh.environments[0];
    assert_eq!(env.environment_id, chain.environment.to_string());
    let members: Vec<(&str, bool)> = env
        .members
        .iter()
        .map(|m| (m.ip.as_str(), m.healthy))
        .collect();
    assert_eq!(members, [("10.80.1.2", true), ("10.80.2.2", true)]);

    // One replica crashes: the other keeps serving, and leaves the balancer.
    let crashed = pb::DeploymentPhase::Crashed;
    deploy::state::apply_event(app.db(), b, &event(started.id, crashed))
        .await
        .unwrap();
    assert_eq!(status(&app, started.id).await, "active");
    let ds = deploy::desired::build(&app.state, a).await.unwrap();
    let route = ds
        .routes
        .iter()
        .find(|r| r.deployment_id == started.id.to_string())
        .unwrap();
    assert!(route.remote_upstreams.is_empty());
    let healthy: Vec<bool> = ds.mesh.unwrap().environments[0]
        .members
        .iter()
        .map(|m| m.healthy)
        .collect();
    assert_eq!(healthy, [true, false]);
    // Both down: crashed.
    deploy::state::apply_event(app.db(), a, &event(started.id, crashed))
        .await
        .unwrap();
    assert_eq!(status(&app, started.id).await, "crashed");
    // Back up: active again.
    deploy::state::apply_event(app.db(), b, &event(started.id, active))
        .await
        .unwrap();
    assert_eq!(status(&app, started.id).await, "active");

    // A new deployment gets fresh addresses next to the serving one's, and
    // stays on the same two servers.
    let next = deploy(&app, chain.environment, chain.instance, 2)
        .await
        .unwrap();
    let ips: Vec<(Id, String)> = sqlx::query_as(
        "SELECT node_id, mesh_ip FROM deployment_replicas WHERE deployment_id = $1 ORDER BY ordinal",
    )
    .bind(next.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        ips,
        [(a, "10.80.1.3".to_owned()), (b, "10.80.2.3".to_owned())]
    );
    // Failing anywhere fails it everywhere.
    let failed = pb::DeploymentPhase::Failed;
    let kick = deploy::state::apply_event(app.db(), b, &event(next.id, failed))
        .await
        .unwrap();
    assert_eq!(status(&app, next.id).await, "failed");
    assert!(kick.contains(&a) && kick.contains(&b));

    // With a volume, one replica only.
    sqlx::query(
        "INSERT INTO volumes (id, service_instance_id, mount_path) VALUES ($1, $2, '/data')",
    )
    .bind(Id::new())
    .bind(chain.instance)
    .execute(app.db())
    .await
    .unwrap();
    let err = deploy(&app, chain.environment, chain.instance, 2)
        .await
        .unwrap_err();
    assert!(err.contains("volume"), "{err}");
    assert!(
        deploy(&app, chain.environment, chain.instance, 1)
            .await
            .is_ok()
    );
}
