//! Functions on the servers, end to end over real gRPC + mTLS: the node
//! runs them in its workerd (a fake one here), routes their domains to it,
//! replaces a function on redeploy, and the API refuses what a function
//! can't be.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_node::edge::RecordingEdge;
use modula_node::functions::{FakeWorkerd, Functions};
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const HELLO: &str = "export default { fetch(req, env) { return new Response(env.GREETING); } };";

struct Cluster {
    app: TestApp,
    admin: String,
    fns: Arc<Functions>,
    fake: Arc<FakeWorkerd>,
    edge: Arc<RecordingEdge>,
    rt: Arc<FakeRuntime>,
    stop: CancellationToken,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
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
    let t = app
        .call(Method::POST, "/api/nodes/join-tokens", Some(&admin), None)
        .await
        .json;
    let dir = std::env::temp_dir().join(format!("modula-functions-test-{}", Id::new()));
    join(JoinArgs {
        server: &format!("https://localhost:{port}"),
        token: t["token"].as_str().unwrap(),
        fingerprint: t["fingerprint"].as_str().unwrap(),
        hostname: "edge-1".into(),
        dir: &dir,
    })
    .await
    .expect("join");
    let fake = Arc::new(FakeWorkerd::default());
    let fns = Functions::new(dir.join("functions"), fake.clone()).unwrap();
    let edge = Arc::new(RecordingEdge::default());
    let rt = Arc::new(FakeRuntime::new());
    tokio::spawn(modula_node::session::run(
        dir,
        rt.clone(),
        modula_node::reconcile::Capabilities {
            edge: Some(edge.clone()),
            functions: Some(fns.clone()),
            ..Default::default()
        },
        stop.child_token(),
    ));
    let c = Cluster {
        app,
        admin,
        fns,
        fake,
        edge,
        rt,
        stop,
    };
    wait_for("node online", || async {
        let r = c.call(Method::GET, "/api/nodes", None).await;
        r.json
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["connected"] == true)
    })
    .await;
    c
}

impl Cluster {
    async fn call(&self, m: Method, path: &str, body: Option<Value>) -> Resp {
        self.app.call(m, path, Some(&self.admin), body).await
    }

    /// A function service in a new workspace; returns its instance.
    async fn function(&self, config: Value) -> Resp {
        let ws = self.app.create_workspace(&self.admin, "team").await;
        let p = self
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(json!({"name": "api"})),
            )
            .await;
        let project = p.json["id"].as_str().unwrap().to_owned();
        self.call(
            Method::POST,
            &format!("/api/projects/{project}/services"),
            Some(json!({ "name": "hello", "config": config })),
        )
        .await
    }

    async fn deploy_active(&self, instance: &str) -> String {
        let r = self
            .call(
                Method::POST,
                &format!("/api/service-instances/{instance}/deployments"),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let id = r.json["id"].as_str().unwrap().to_owned();
        let mut last = Value::Null;
        for _ in 0..200 {
            last = self
                .call(Method::GET, &format!("/api/deployments/{id}"), None)
                .await
                .json;
            if last["status"] == "active" {
                return id;
            }
            assert_ne!(last["status"], "failed", "{last}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("never active: {last}");
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn functions_run_in_the_nodes_workerd(pool: PgPool) {
    let c = cluster(pool).await;
    let s = c
        .function(json!({
            "source": { "type": "function", "code": HELLO, "compatibilityDate": "2026-10-01" },
            "healthcheckPath": "/"
        }))
        .await;
    assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
    let instance = s.json["instances"][0]["id"].as_str().unwrap().to_owned();
    let r = c
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}/variables"),
            Some(json!({ "set": { "GREETING": "hello \"there\"" } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    sqlx::query(
        "INSERT INTO domains (id, service_instance_id, hostname, kind) VALUES ($1, $2, 'hello.apps.test', 'generated')",
    )
    .bind(Id::new())
    .bind(instance.parse::<Id>().unwrap())
    .execute(c.app.db())
    .await
    .unwrap();

    let first = c.deploy_active(&instance).await;
    assert!(c.fns.contains(&first));
    assert!(c.rt.running_deployments().is_empty(), "no container");
    let config = c.fake.launches().last().unwrap().clone();
    assert!(
        config.contains("(name = \"GREETING\", text = \"hello \\\"there\\\"\")"),
        "{config}"
    );
    let d = c
        .call(Method::GET, &format!("/api/deployments/{first}"), None)
        .await
        .json;
    assert_eq!(d["sourceKind"], "function", "{d}");

    // Its domain goes to its socket in workerd.
    let port = c.fns.port(&first).unwrap();
    wait_for("the route", || async {
        c.edge.last.lock().unwrap().as_ref().is_some_and(|cfg| {
            cfg.routes.iter().any(|r| {
                r.hostname == "hello.apps.test" && r.upstream == ([127, 0, 0, 1], port).into()
            })
        })
    })
    .await;

    // A new version replaces it.
    let r = c
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}"),
            Some(json!({ "config": {
                "source": { "type": "function", "code": HELLO.replace("GREETING", "OTHER"), "compatibilityDate": "2026-10-01" }
            }})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let second = c.deploy_active(&instance).await;
    wait_for("the old function to go", || async {
        !c.fns.contains(&first) && c.fns.contains(&second)
    })
    .await;
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn what_a_function_cant_be_is_refused(pool: PgPool) {
    let c = cluster(pool).await;
    for (config, why) in [
        (
            json!({ "source": { "type": "function", "compatibilityDate": "2026-10-01" } }),
            "no code",
        ),
        (
            json!({ "source": { "type": "function", "code": HELLO, "compatibilityDate": "2099-01-01" } }),
            "a date workerd doesn't know",
        ),
        (
            json!({ "source": { "type": "function", "code": HELLO, "compatibilityDate": "2026-10-01" },
                    "cronSchedule": "@daily" }),
            "a schedule",
        ),
    ] {
        let r = c.function(config).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{why}: {:?}", r.json);
    }
}
