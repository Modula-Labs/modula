//! End to end over real gRPC + mTLS: a node joins with a single-use token,
//! attaches, and runs deployments created through the HTTP API (on an
//! in-memory runtime). Covers the deployment state machine, redeploys, live
//! logs with secret redaction, and revocation.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_node::edge::RecordingEdge;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct Cluster {
    app: TestApp,
    admin: String,
    server_url: String,
    edge: Arc<RecordingEdge>,
    stop: CancellationToken,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn cluster(pool: PgPool) -> Cluster {
    cluster_with(pool, config()).await
}

async fn cluster_with(pool: PgPool, cfg: modula_server::config::Config) -> Cluster {
    let app = TestApp::with_config(pool, cfg);
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
        edge: Arc::new(RecordingEdge::default()),
        stop,
    }
}

fn temp_dir() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("modula-node-test-{}", Id::new()))
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
    /// The upstream the node's edge currently routes `host` to.
    fn routed(&self, host: &str) -> Option<std::net::SocketAddr> {
        let last = self.edge.last.lock().unwrap();
        last.as_ref()?
            .routes
            .iter()
            .find(|r| r.hostname == host)
            .map(|r| r.upstream)
    }

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
    async fn start_node(&self) -> (Id, Arc<FakeRuntime>, CancellationToken) {
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
        let caps = modula_node::reconcile::Capabilities {
            edge: Some(self.edge.clone()),
            build: Some(modula_node::build::BuildSupport {
                builder: Arc::new(modula_node::build::FakeBuilder::default()),
                logs: modula_node::build::BuildLogs::new(dir.join("build-logs")).unwrap(),
            }),
            db: None,
            upgrade: None,
            mesh: None,
        };
        tokio::spawn(modula_node::session::run(
            dir,
            rt.clone(),
            caps,
            stop.clone(),
        ));
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
    async fn service(&self, image: &str) -> Id {
        self.service_with(json!({
            "source": { "type": "image", "image": image }, "port": 8080, "healthcheckPath": "/health"
        }))
        .await
    }

    async fn service_with(&self, config: Value) -> Id {
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
                Some(json!({ "name": "api", "config": config })),
            )
            .await;
        assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
        s.json["instances"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn deploy(&self, instance: Id) -> Id {
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

    async fn status(&self, deployment: Id) -> String {
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

    async fn wait_status(&self, deployment: Id, want: &str) {
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
    let command = t["command"].as_str().unwrap();
    assert!(command.starts_with(
        "curl -fsSL https://cloud.test/api/install.sh | sudo bash -s -- join --server "
    ));
    assert!(command.ends_with(&format!("--token {token} --ca {fp}")));
    assert!(
        t["nodeCommand"]
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
            Some(json!({ "set": { "API_KEY": "sk_test_fake_abcdef123456", "MODE": "production" }, "sealed": ["API_KEY"] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(
        !r.json.to_string().contains("sk_test_fake"),
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
        !body.contains("sk_test_fake_abcdef123456"),
        "secret leaked into logs: {body}"
    );
    assert!(body.contains("event: end"));

    // A generated domain is routed to the serving container.
    let r = c
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/domains"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let host = r.json["hostname"].as_str().unwrap().to_owned();
    assert!(
        host.starts_with("api-") && host.ends_with(".apps.test"),
        "{host}"
    );
    wait_for("route to d1", || async { c.routed(&host).is_some() }).await;
    let d1_upstream = c.routed(&host).unwrap();

    // A redeploy replaces the running deployment only once it is healthy.
    let d2 = c.deploy(instance).await;
    c.wait_status(d2, "active").await;
    c.wait_status(d1, "removed").await;
    wait_for("old container stopped", || async {
        rt.running_deployments() == [d2.to_string()].into()
    })
    .await;
    wait_for("route moved to d2", || async {
        c.routed(&host).is_some_and(|u| u != d1_upstream)
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
    let other_node = Id::new();
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

/// A stand-in for GitHub's REST API: one repository with one branch.
async fn mock_github() -> url::Url {
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/repos/acme/app/commits/{branch}",
        get(
            |axum::extract::Path(branch): axum::extract::Path<String>| async move {
                if branch != "main" {
                    return (
                        StatusCode::NOT_FOUND,
                        axum::Json(json!({"message": "No commit found"})),
                    );
                }
                (
                    StatusCode::OK,
                    axum::Json(json!({
                        "sha": "0123456789abcdef0123456789abcdef01234567",
                        "commit": { "message": "Add checkout", "author": { "name": "Ada" } }
                    })),
                )
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}").parse().unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn repository_deploys_build_on_the_node(pool: PgPool) {
    let mut cfg = config();
    cfg.github_api = mock_github().await;
    let c = cluster_with(pool, cfg).await;
    let (_node, rt, _stop) = c.start_node().await;
    let instance = c
        .service_with(json!({
            "source": { "type": "repo", "repo": "acme/app", "branch": "main" },
            "port": 8080
        }))
        .await;

    let d = c.deploy(instance).await;
    let r = c
        .app
        .call(
            Method::GET,
            &format!("/api/deployments/{d}"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(
        r.json["commitSha"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(r.json["commitMessage"], "Add checkout");
    assert_eq!(r.json["source"], "acme/app@main");
    assert!(
        r.json["image"]
            .as_str()
            .unwrap()
            .starts_with("modula-build/")
    );

    c.wait_status(d, "active").await;
    let r = c
        .app
        .call(
            Method::GET,
            &format!("/api/deployments/{d}"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.json["imageDigest"], format!("sha256:{}", "ab".repeat(32)));
    assert!(r.json["buildFinishedAt"].is_string());
    let phases: Vec<&str> = r.json["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["phase"].as_str().unwrap())
        .collect();
    assert!(
        phases.starts_with(&["queued", "building", "built"]),
        "{phases:?}"
    );
    assert!(
        !phases.contains(&"pulling"),
        "built images are never pulled"
    );
    assert_eq!(rt.running_deployments().len(), 1);

    // The build log streams through the same endpoint as container logs.
    let (status, body) = c
        .app
        .get_text(
            &format!("/api/deployments/{d}/logs?kind=build&follow=false"),
            &c.admin,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("Build complete"), "{body}");

    // An unknown branch is a clear error, not a queued deployment.
    let other = c
        .service_with(json!({ "source": { "type": "repo", "repo": "acme/app", "branch": "nope" } }))
        .await;
    let r = c
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{other}/deployments"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn rollback_rebuilds_the_commit_and_restart_is_in_place(pool: PgPool) {
    use modula_node::runtime::Runtime;

    let mut cfg = config();
    cfg.github_api = mock_github().await;
    let c = cluster_with(pool, cfg).await;
    let (_node, rt, _stop) = c.start_node().await;
    let instance = c
        .service_with(json!({ "source": { "type": "repo", "repo": "acme/app", "branch": "main" } }))
        .await;
    let first = c.deploy(instance).await;
    c.wait_status(first, "active").await;

    let post = |action: &'static str, d: Id| {
        let app = &c.app;
        let admin = c.admin.clone();
        async move {
            app.call(
                Method::POST,
                &format!("/api/deployments/{d}/{action}"),
                Some(&admin),
                None,
            )
            .await
        }
    };

    // Restart: same deployment, the container restarts in place.
    let r = post("restart", first).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    wait_for("container restarted", || async {
        rt.containers()
            .await
            .unwrap()
            .iter()
            .any(|c| c.restart_count == 1)
    })
    .await;
    assert_eq!(c.status(first).await, "active");

    // Rollback: a new deployment that rebuilds the same commit.
    let r = post("rollback", first).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["trigger"], "rollback");
    assert_eq!(r.json["sourceDeploymentId"], first.to_string());
    assert_eq!(
        r.json["commitSha"],
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(r.json["status"], "building");
    let second: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    c.wait_status(second, "active").await;
    c.wait_status(first, "removed").await;

    // Only running deployments restart.
    assert_eq!(post("restart", first).await.status, StatusCode::CONFLICT);

    // Image deployments redeploy but can't be rolled back to.
    let image = c.service("nginx:alpine").await;
    let d = c.deploy(image).await;
    assert_eq!(post("rollback", d).await.status, StatusCode::CONFLICT);
    let r = post("redeploy", d).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["trigger"], "redeploy");
    assert_eq!(r.json["sourceKind"], "image");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn references_resolve_across_services_and_edges_follow(pool: PgPool) {
    let c = cluster(pool).await;
    let (_node, rt, _stop) = c.start_node().await;
    // `service_with` makes a project per call; build two services in one.
    let api_instance = c
        .service_with(json!({ "source": { "type": "image", "image": "app:1" }, "port": 3000 }))
        .await;
    let call = |m: Method, path: String, body: Option<Value>| {
        let app = &c.app;
        let admin = c.admin.clone();
        async move { app.call(m, &path, Some(&admin), body).await }
    };
    let inst = call(
        Method::GET,
        format!("/api/service-instances/{api_instance}"),
        None,
    )
    .await;
    let (project, env) = (
        inst.json["projectId"].as_str().unwrap().to_owned(),
        inst.json["environmentId"].as_str().unwrap().to_owned(),
    );
    let db = call(
        Method::POST,
        format!("/api/projects/{project}/services"),
        Some(json!({ "name": "db", "config": { "source": { "type": "image", "image": "postgres:17" }, "port": 5432 } })),
    )
    .await;
    let db_id = db.json["id"].as_str().unwrap().to_owned();
    let db_instance = db.json["instances"][0]["id"].as_str().unwrap().to_owned();

    let r = call(
        Method::PATCH,
        format!("/api/service-instances/{db_instance}/variables"),
        Some(json!({ "set": { "PASSWORD": "s3cret-pw" } })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = call(
        Method::PATCH,
        format!("/api/environments/{env}/variables"),
        Some(json!({ "set": { "DB": "shop" } })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let url = "postgres://app:${{db.PASSWORD}}@${{db.MODULA_PRIVATE_DOMAIN}}:${{db.PORT}}/${{shared.DB}}?x=${{nope.X}}";
    let r = call(
        Method::PATCH,
        format!("/api/service-instances/{api_instance}/variables"),
        Some(json!({ "set": { "DATABASE_URL": url, "SELF": "${{PORT}}" } })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let refs: Vec<&str> = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "DATABASE_URL")
        .unwrap()["references"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        refs,
        [
            "db.PASSWORD",
            "db.MODULA_PRIVATE_DOMAIN",
            "db.PORT",
            "shared.DB",
            "nope.X"
        ]
    );

    // A loop through two services is refused.
    let r = call(
        Method::PATCH,
        format!("/api/service-instances/{db_instance}/variables"),
        Some(json!({ "set": { "BACK": "${{api.LOOP}}" } })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = call(
        Method::PATCH,
        format!("/api/service-instances/{api_instance}/variables"),
        Some(json!({ "set": { "LOOP": "${{db.BACK}}" } })),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("api.LOOP → db.BACK → api.LOOP")
    );

    // The canvas edge: api depends on db.
    let p = call(Method::GET, format!("/api/projects/{project}"), None).await;
    assert_eq!(p.status, StatusCode::OK, "{:?}", p.json);
    let api_summary = p.json["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == api_instance.to_string())
        .unwrap();
    assert_eq!(api_summary["dependsOn"], json!([db_id]));

    // Everything is pending; deploying the environment deploys both.
    let pending = call(
        Method::GET,
        format!("/api/environments/{env}/pending"),
        None,
    )
    .await;
    assert_eq!(
        pending.json["services"].as_array().unwrap().len(),
        2,
        "{:?}",
        pending.json
    );
    let r = call(
        Method::POST,
        format!("/api/environments/{env}/deploy"),
        Some(json!({})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let deployed = r.json["deployments"].as_array().unwrap().clone();
    assert_eq!(deployed.len(), 2);
    let api_dep = deployed
        .iter()
        .find(|d| d["serviceId"] != db_id.as_str())
        .unwrap()["deploymentId"]
        .as_str()
        .unwrap()
        .to_owned();
    c.wait_status(api_dep.parse().unwrap(), "active").await;
    let env_vars = rt.env_of(&api_dep);
    assert_eq!(
        env_vars["DATABASE_URL"],
        "postgres://app:s3cret-pw@db.modula.internal:5432/shop?x="
    );
    assert_eq!(env_vars["SELF"], "3000");
    assert_eq!(env_vars["PORT"], "3000");
    let d = call(Method::GET, format!("/api/deployments/{api_dep}"), None).await;
    assert!(
        d.json["events"].to_string().contains("nope.X"),
        "missing references are reported"
    );
    let pending = call(
        Method::GET,
        format!("/api/environments/{env}/pending"),
        None,
    )
    .await;
    assert_eq!(pending.json["count"], 0);

    // A shared variable change is pending for the services that use it.
    call(
        Method::PATCH,
        format!("/api/environments/{env}/variables"),
        Some(json!({ "set": { "DB": "shop2" } })),
    )
    .await;
    let pending = call(
        Method::GET,
        format!("/api/environments/{env}/pending"),
        None,
    )
    .await;
    let names: Vec<&str> = pending.json["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["serviceName"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api"]);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn database_templates_get_a_volume_a_proxy_and_stop_before_start(pool: PgPool) {
    let mut cfg = config();
    cfg.tcp_proxy_host = Some("db.example.test".into());
    let c = cluster_with(pool, cfg).await;
    let (_node, rt, _stop) = c.start_node().await;
    let call = |m: Method, path: String, body: Option<Value>| {
        let app = &c.app;
        let admin = c.admin.clone();
        async move { app.call(m, &path, Some(&admin), body).await }
    };
    // A project with a plain service, then a Postgres from the template.
    let web = c.service("app:1").await;
    let inst = call(Method::GET, format!("/api/service-instances/{web}"), None).await;
    let project = inst.json["projectId"].as_str().unwrap().to_owned();
    let r = call(
        Method::POST,
        format!("/api/projects/{project}/services"),
        Some(json!({ "name": "postgres", "template": "postgres" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let pg = &r.json["instances"][0];
    let pg_id = pg["id"].as_str().unwrap().to_owned();
    assert_eq!(pg["template"], "postgres");
    assert_eq!(pg["volume"]["mountPath"], "/var/lib/postgresql/data");
    let volume_id = pg["volume"]["id"].as_str().unwrap().to_owned();

    // Public TCP access, then deploy: the URLs resolve, the volume is mounted.
    let r = call(
        Method::POST,
        format!("/api/service-instances/{pg_id}/tcp-proxy"),
        Some(json!({ "targetPort": 5432 })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["tcpProxy"]["publicPort"], 20000);
    assert_eq!(r.json["tcpProxy"]["host"], "db.example.test");
    let first = c.deploy(pg_id.parse().unwrap()).await;
    c.wait_status(first, "active").await;
    let env = rt.env_of(&first.to_string());
    let password = env["POSTGRES_PASSWORD"].clone();
    assert_eq!(password.len(), 32);
    assert_eq!(
        env["DATABASE_URL"],
        format!("postgresql://postgres:{password}@postgres.modula.internal:5432/app")
    );
    assert_eq!(
        env["DATABASE_PUBLIC_URL"],
        format!("postgresql://postgres:{password}@db.example.test:20000/app?sslmode=require")
    );
    assert_eq!(
        rt.mount_of(&first.to_string()),
        Some((
            format!("modula-vol-{volume_id}"),
            "/var/lib/postgresql/data".into()
        ))
    );

    // A redeploy never overlaps on the volume: the old one goes first.
    let second = c.deploy(pg_id.parse().unwrap()).await;
    assert_eq!(c.status(first).await, "removed");
    c.wait_status(second, "active").await;
    assert!(!rt.running_deployments().contains(&first.to_string()));

    // Deleting the service deletes the volume's data on the node.
    let svc = r.json["serviceId"].as_str().unwrap().to_owned();
    let d = call(Method::DELETE, format!("/api/services/{svc}"), None).await;
    assert_eq!(d.status, StatusCode::OK, "{:?}", d.json);
    wait_for("volume removed", || async {
        rt.removed_volumes()
            .contains(&format!("modula-vol-{volume_id}"))
    })
    .await;
}

/// Deploy settings end to end: a pre-deploy command runs from
/// the new image with the deployment's variables before it starts, and its
/// failure (like a failing health check) leaves the serving deployment alone;
/// draining and restart limits reach the container.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn pre_deploy_draining_retries_and_health_gating(pool: PgPool) {
    let c = cluster(pool).await;
    let (_node, rt, _stop) = c.start_node().await;
    let migrate = ["bunx", "prisma", "migrate", "deploy"];
    let config = json!({
        "source": { "type": "image", "image": "ghcr.io/acme/anton-api:1" },
        "port": 3000,
        "healthcheckPath": "/v1/ready",
        "healthcheckTimeoutSecs": 120,
        "restartPolicyMaxRetries": 5,
        "preDeployCommand": migrate,
    });
    let instance = c.service_with(config.clone()).await;
    let call = |m: Method, path: String, body: Option<Value>| {
        let app = &c.app;
        let admin = c.admin.clone();
        async move { app.call(m, &path, Some(&admin), body).await }
    };
    // Railway's variable sets the draining time (the config has none).
    let r = call(
        Method::PATCH,
        format!("/api/service-instances/{instance}/variables"),
        Some(json!({ "set": {
            "DATABASE_URL": "postgres://u:pw@postgres.modula.internal:5432/app",
            "RAILWAY_DEPLOYMENT_DRAINING_SECONDS": "30",
            "RELEASE": "${{MODULA_DEPLOYMENT_ID}}"
        } })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = call(
        Method::POST,
        format!("/api/service-instances/{instance}/domains"),
        None,
    )
    .await;
    let host = r.json["hostname"].as_str().unwrap().to_owned();

    let d1 = c.deploy(instance).await;
    c.wait_status(d1, "active").await;
    let once = rt.onces.lock().unwrap()[0].clone();
    assert_eq!(once.deployment_id, d1.to_string());
    assert_eq!(once.cmd, migrate);
    assert_eq!(once.image, "ghcr.io/acme/anton-api:1");
    assert_eq!(
        once.env["DATABASE_URL"],
        "postgres://u:pw@postgres.modula.internal:5432/app"
    );
    assert_eq!(
        once.env["RELEASE"],
        d1.to_string(),
        "its own new deployment"
    );
    assert!(
        once.running.is_empty(),
        "it ran before the container started"
    );
    let w = rt.workload_of(&d1.to_string()).unwrap();
    assert_eq!(w.draining_seconds, 30);
    assert_eq!(w.restart_max_retries, 5);
    assert_eq!(w.health.unwrap().timeout_secs, 120);
    wait_for("route to d1", || async { c.routed(&host).is_some() }).await;
    let upstream = c.routed(&host).unwrap();

    // The migration fails: the new deployment fails, d1 keeps serving.
    rt.once_exit.lock().unwrap().insert(migrate.join(" "), 1);
    let d2 = c.deploy(instance).await;
    c.wait_status(d2, "failed").await;
    assert_eq!(c.status(d1).await, "active");
    let detail = call(Method::GET, format!("/api/deployments/{d2}"), None).await;
    let error = detail.json["error"].as_str().unwrap();
    assert!(
        error.starts_with("Pre-deploy command exited with code 1"),
        "{error}"
    );
    assert!(
        detail.json["events"]
            .to_string()
            .contains("Running pre-deploy command: bunx prisma migrate deploy")
    );
    // Its output is in the deployment's log.
    let (_, body) = c
        .app
        .get_text(
            &format!("/api/deployments/{d2}/logs?kind=build&follow=false"),
            &c.admin,
        )
        .await;
    assert!(body.contains("ran bunx prisma migrate deploy"), "{body}");
    assert_eq!(rt.running_deployments(), [d1.to_string()].into());

    // Health gating: a deployment that never returns 2xx in time fails, and
    // traffic never leaves d1.
    rt.once_exit.lock().unwrap().clear();
    rt.unhealthy.lock().unwrap().insert("*".into());
    let mut quick = config.clone();
    quick["healthcheckTimeoutSecs"] = json!(1);
    let r = call(
        Method::PATCH,
        format!("/api/service-instances/{instance}"),
        Some(json!({ "config": quick })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let d3 = c.deploy(instance).await;
    c.wait_status(d3, "failed").await;
    assert_eq!(c.status(d1).await, "active");
    assert_eq!(c.routed(&host), Some(upstream));
    assert_eq!(rt.running_deployments(), [d1.to_string()].into());

    // Out-of-range settings are refused.
    for bad in [
        json!({ "drainingSeconds": 301 }),
        json!({ "restartPolicyMaxRetries": 0 }),
        json!({ "restartPolicy": "always", "restartPolicyMaxRetries": 3 }),
    ] {
        let mut cfg = config.clone();
        cfg.as_object_mut()
            .unwrap()
            .extend(bad.as_object().unwrap().clone());
        let r = call(
            Method::PATCH,
            format!("/api/service-instances/{instance}"),
            Some(json!({ "config": cfg })),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}: {:?}", r.json);
    }
}
