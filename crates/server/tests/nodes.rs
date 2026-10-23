//! End to end over real gRPC + mTLS: a node joins with a single-use token,
//! attaches, and runs deployments created through the HTTP API (on an
//! in-memory runtime). Covers the deployment state machine, redeploys, live
//! logs with secret redaction, and revocation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Cluster {
    app: TestApp,
    admin: String,
    server_url: String,
    stop: CancellationToken,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn cluster(pool: PgPool) -> Cluster {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = CancellationToken::new();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        listener,
        stop.clone(),
    ));
    Cluster {
        app,
        admin,
        server_url: format!("https://localhost:{port}"),
        stop,
    }
}

fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("modula-node-test-{}", Uuid::new_v4()))
}

async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

impl Cluster {
    async fn join_token(&self) -> Value {
        let r = self
            .app
            .call(
                Method::POST,
                "/api/nodes/join-tokens",
                Some(&self.admin),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json
    }

    /// Join a node and start its session on a fake runtime.
    async fn start_node(&self) -> (Uuid, Arc<FakeRuntime>, CancellationToken) {
        let t = self.join_token().await;
        let dir = temp_dir();
        let id = join(JoinArgs {
            server: &self.server_url,
            token: t["token"].as_str().unwrap(),
            fingerprint: t["fingerprint"].as_str().unwrap(),
            hostname: "worker-1".into(),
            dir: &dir,
        })
        .await
        .expect("join");
        let rt = Arc::new(FakeRuntime::new());
        let stop = self.stop.child_token();
        tokio::spawn(modula_node::session::run(dir, rt.clone(), stop.clone()));
        let node = id.meta.node_id;
        wait_for("node online", || async {
            let r = self
                .app
                .call(Method::GET, "/api/nodes", Some(&self.admin), None)
                .await;
            r.json
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["id"] == node.to_string() && n["connected"] == true)
        })
        .await;
        (node, rt, stop)
    }

    /// Workspace → project → service; returns the production instance id.
    async fn service(&self, image: &str) -> Uuid {
        let ws = self.app.create_workspace(&self.admin, "team").await;
        let p = self
            .app
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(&self.admin),
                Some(json!({"name": "shop"})),
            )
            .await;
        assert_eq!(p.status, StatusCode::OK, "{:?}", p.json);
        let project = p.json["id"].as_str().unwrap().to_owned();
        let s = self
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{project}/services"),
                Some(&self.admin),
                Some(json!({
                    "name": "api",
                    "config": { "source": { "type": "image", "image": image }, "port": 8080, "healthcheckPath": "/health" }
                })),
            )
            .await;
        assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
        s.json["instances"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn deploy(&self, instance: Uuid) -> Uuid {
        let r = self
            .app
            .call(
                Method::POST,
                &format!("/api/service-instances/{instance}/deployments"),
                Some(&self.admin),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json["id"].as_str().unwrap().parse().unwrap()
    }

    async fn status(&self, deployment: Uuid) -> String {
        let r = self
            .app
            .call(
                Method::GET,
                &format!("/api/deployments/{deployment}"),
                Some(&self.admin),
                None,
            )
            .await;
        r.json["status"].as_str().unwrap_or_default().to_owned()
    }

    async fn wait_status(&self, deployment: Uuid, want: &str) {
        wait_for(&format!("deployment {want}"), || async {
            self.status(deployment).await == want
        })
        .await;
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn join_tokens_are_single_use_and_the_ca_is_pinned(pool: PgPool) {
    let c = cluster(pool).await;
    let t = c.join_token().await;
    let token = t["token"].as_str().unwrap();
    let fp = t["fingerprint"].as_str().unwrap();
    assert!(
        t["command"]
            .as_str()
            .unwrap()
            .starts_with("modula-node join --server ")
    );

    // A wrong pin refuses the server before the token is ever sent.
    let wrong = format!("sha256:{}", "0".repeat(64));
    let err = join(JoinArgs {
        server: &c.server_url,
        token,
        fingerprint: &wrong,
        hostname: "x".into(),
        dir: &temp_dir(),
    })
    .await
    .err()
    .expect("a wrong pin must fail");
    assert!(format!("{err:#}").contains("cannot reach"), "{err:#}");

    join(JoinArgs {
        server: &c.server_url,
        token,
        fingerprint: fp,
        hostname: "x".into(),
        dir: &temp_dir(),
    })
    .await
    .expect("first use");
    let err = join(JoinArgs {
        server: &c.server_url,
        token,
        fingerprint: fp,
        hostname: "x".into(),
        dir: &temp_dir(),
    })
    .await
    .err()
    .expect("second use must fail");
    assert!(
        format!("{err:#}").contains("invalid or expired join token"),
        "{err:#}"
    );

    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'node.enroll'")
            .fetch_one(c.app.db())
            .await
            .unwrap();
    assert_eq!(audited, 1);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn deploy_redeploy_logs_and_revoke(pool: PgPool) {
    let c = cluster(pool).await;
    let instance = c.service("ghcr.io/acme/api:1").await;

    // Without a server there is nowhere to deploy.
    let r = c
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/deployments"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    let (node, rt, _node_stop) = c.start_node().await;
    let r = c
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}/variables"),
            Some(&c.admin),
            Some(json!({ "set": { "API_KEY": "sk_live_abcdef123456", "MODE": "production" }, "sealed": ["API_KEY"] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(
        !r.json.to_string().contains("sk_live"),
        "values are never returned"
    );

    let d1 = c.deploy(instance).await;
    c.wait_status(d1, "active").await;
    assert_eq!(rt.running_deployments(), [d1.to_string()].into());

    // Logs come from the node, with secret values masked.
    let (status, body) = c
        .app
        .get_text(
            &format!("/api/deployments/{d1}/logs?follow=false&tail=50"),
            &c.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("started"), "{body}");
    assert!(body.contains("API_KEY=••••••"), "{body}");
    assert!(
        !body.contains("sk_live_abcdef123456"),
        "secret leaked into logs: {body}"
    );
    assert!(body.contains("event: end"));

    // A redeploy replaces the running deployment only once it is healthy.
    let d2 = c.deploy(instance).await;
    c.wait_status(d2, "active").await;
    c.wait_status(d1, "removed").await;
    wait_for("old container stopped", || async {
        rt.running_deployments() == [d2.to_string()].into()
    })
    .await;

    // A crash is reported and recovery flips it back.
    rt.crash(&d2.to_string(), 137);
    c.wait_status(d2, "crashed").await;
    rt.recover(&d2.to_string());
    c.wait_status(d2, "active").await;

    // A failing deployment does not take down the live one.
    let r = c
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}"),
            Some(&c.admin),
            Some(json!({ "config": { "source": { "type": "image", "image": "ghcr.io/acme/missing:9" }, "port": 8080 } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    rt.unpullable
        .lock()
        .unwrap()
        .insert("ghcr.io/acme/missing:9".into());
    let d3 = c.deploy(instance).await;
    c.wait_status(d3, "failed").await;
    assert_eq!(c.status(d2).await, "active");
    let detail = c
        .app
        .call(
            Method::GET,
            &format!("/api/deployments/{d3}"),
            Some(&c.admin),
            None,
        )
        .await;
    assert!(
        detail.json["error"]
            .as_str()
            .unwrap()
            .contains("Image pull failed"),
        "{:?}",
        detail.json
    );

    // Revoking the server disconnects it and stops its deployments.
    let r = c
        .app
        .call(
            Method::DELETE,
            &format!("/api/nodes/{node}"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(c.status(d2).await, "removed");
    wait_for("node disconnected", || async {
        !c.app.state.hub.is_connected(node)
    })
    .await;
    // It cannot come back with its old certificate.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!c.app.state.hub.is_connected(node));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_node_cannot_report_on_another_nodes_deployments(pool: PgPool) {
    let c = cluster(pool).await;
    let ws = c.app.create_workspace(&c.admin, "team").await;
    let chain = c.app.seed_chain(ws).await;
    sqlx::query("UPDATE deployments SET status = 'deploying' WHERE id = $1")
        .bind(chain.deployment)
        .execute(c.app.db())
        .await
        .unwrap();
    let ev = modula_proto::node::v1::DeploymentEvent {
        deployment_id: chain.deployment.to_string(),
        phase: modula_proto::node::v1::DeploymentPhase::Failed as i32,
        message: "forged".into(),
        ..Default::default()
    };
    let other_node = Uuid::now_v7();
    modula_server::deploy::state::apply_event(c.app.db(), other_node, &ev)
        .await
        .unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM deployments WHERE id = $1")
        .bind(chain.deployment)
        .fetch_one(c.app.db())
        .await
        .unwrap();
    assert_eq!(status, "deploying");
}
