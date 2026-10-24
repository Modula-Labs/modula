//! Cron services end to end over real gRPC + mTLS (in-memory runtime): the
//! deployment becomes active without a container, due times are claimed
//! once, each run is a one-off container whose output lands in the logs,
//! a due time while one runs is skipped, runs time out, and who may see and
//! start them.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use modula_server::cron;
use serde_json::{Value, json};
use sqlx::PgPool;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

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
    async fn call(&self, m: Method, path: &str, body: Option<Value>) -> Resp {
        self.app.call(m, path, Some(&self.admin), body).await
    }

    async fn start_node(&self) -> Arc<FakeRuntime> {
        let t = self
            .call(Method::POST, "/api/nodes/join-tokens", None)
            .await
            .json;
        let dir = std::env::temp_dir().join(format!("modula-cron-test-{}", Id::new()));
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
        tokio::spawn(modula_node::session::run(
            dir,
            rt.clone(),
            modula_node::reconcile::Capabilities::default(),
            self.stop.child_token(),
        ));
        let node = id.meta.node_id.to_string();
        wait_for("node online", || async {
            let r = self.call(Method::GET, "/api/nodes", None).await;
            r.json
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["id"] == node && n["connected"] == true)
        })
        .await;
        rt
    }

    /// A service in a new workspace; returns (workspace, instance).
    async fn service(&self, config: Value) -> (Id, Id) {
        let ws = self.app.create_workspace(&self.admin, "team").await;
        let p = self
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(json!({"name": "shop"})),
            )
            .await;
        let project = p.json["id"].as_str().unwrap().to_owned();
        let s = self
            .call(
                Method::POST,
                &format!("/api/projects/{project}/services"),
                Some(json!({ "name": "nightly", "config": config })),
            )
            .await;
        assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
        let instance = s.json["instances"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        (ws, instance)
    }

    async fn deploy_active(&self, instance: Id) -> Id {
        let r = self
            .call(
                Method::POST,
                &format!("/api/service-instances/{instance}/deployments"),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let d: Id = r.json["id"].as_str().unwrap().parse().unwrap();
        wait_for("deployment active", || async {
            let r = self
                .call(Method::GET, &format!("/api/deployments/{d}"), None)
                .await;
            r.json["status"] == "active"
        })
        .await;
        d
    }

    async fn runs(&self, instance: Id) -> Value {
        let r = self
            .call(
                Method::GET,
                &format!("/api/service-instances/{instance}/cron-runs"),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json
    }

    async fn wait_run(&self, instance: Id, run: &str, status: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..200 {
            let runs = self.runs(instance).await;
            if let Some(r) = runs["runs"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == run)
            {
                if r["status"] == status {
                    return r.clone();
                }
                last = r.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("run {run} never became {status}: {last}");
    }

    /// Make the instance's next due time a minute ago.
    async fn make_due(&self, instance: Id) {
        let r = sqlx::query(
            "UPDATE cron_schedules SET next_run_at = now() - interval '1 minute'
             WHERE service_instance_id = $1",
        )
        .bind(instance)
        .execute(self.app.db())
        .await
        .unwrap();
        assert_eq!(r.rows_affected(), 1, "the instance has a schedule");
    }
}

fn job(cmd: &[&str]) -> Value {
    json!({
        "source": { "type": "image", "image": "ghcr.io/acme/jobs:1" },
        "startCommand": cmd,
        "cronSchedule": "0 3 * * *",
    })
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn scheduled_and_manual_runs(pool: PgPool) {
    let c = cluster(pool).await;
    let rt = c.start_node().await;
    let (_, instance) = c.service(job(&["node", "nightly.js"])).await;
    let r = c
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}/variables"),
            Some(json!({ "set": { "REPORT_TO": "ops@example.com" } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    // Not deployed: the schedule is previewed from the settings.
    let runs = c.runs(instance).await;
    assert_eq!(runs["schedule"]["deployed"], false);
    assert_eq!(runs["schedule"]["description"], "every day at 03:00 UTC");
    let r = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/cron-runs"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "nothing deployed to run");

    // Active without a container.
    let d = c.deploy_active(instance).await;
    assert!(rt.running_deployments().is_empty());
    assert!(rt.onces.lock().unwrap().is_empty());

    // The tick picks the schedule up; the next due time is tomorrow-ish.
    cron::tick(&c.app.state).await.unwrap();
    let runs = c.runs(instance).await;
    let schedule = &runs["schedule"];
    assert_eq!(schedule["deployed"], true);
    assert_eq!(schedule["timeoutSecs"], 3600);
    let upcoming = schedule["upcoming"].as_array().unwrap();
    assert_eq!(upcoming.len(), 3);
    assert!(upcoming[0].as_str().unwrap().ends_with("03:00:00Z"));
    assert!(
        runs["runs"].as_array().unwrap().is_empty(),
        "nothing due yet"
    );
    let card = c
        .call(
            Method::GET,
            &format!(
                "/api/projects/{}",
                sqlx::query_scalar::<_, Id>(
                    "SELECT s.project_id FROM service_instances i JOIN services s ON s.id = i.service_id WHERE i.id = $1"
                )
                .bind(instance)
                .fetch_one(c.app.db())
                .await
                .unwrap()
            ),
            None,
        )
        .await;
    let summary = &card.json["instances"][0];
    assert_eq!(summary["cronSchedule"], "0 3 * * *");
    assert!(summary["cronNextAt"].is_string());

    // Due: one run, from the deployment's image with its variables.
    c.make_due(instance).await;
    cron::tick(&c.app.state).await.unwrap();
    let runs = c.runs(instance).await;
    let first = runs["runs"][0].clone();
    assert_eq!(first["trigger"], "schedule");
    assert!(first["scheduledFor"].is_string());
    let first_id = first["id"].as_str().unwrap();
    let done = c.wait_run(instance, first_id, "succeeded").await;
    assert_eq!(done["exitCode"], 0);
    assert_eq!(done["deploymentId"], d.to_string());
    let once = rt.onces.lock().unwrap()[0].clone();
    assert_eq!(once.cmd, ["node", "nightly.js"]);
    assert_eq!(once.image, "ghcr.io/acme/jobs:1");
    assert_eq!(once.env["REPORT_TO"], "ops@example.com");
    // Its output is stored with the deployment's logs.
    wait_for("run output in the logs", || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM log_lines WHERE deployment_id = $1 AND line = 'ran node nightly.js'",
        )
        .bind(d)
        .fetch_one(c.app.db())
        .await
        .unwrap()
            == 1
    })
    .await;

    // The due time moved on: another tick starts nothing.
    cron::tick(&c.app.state).await.unwrap();
    assert_eq!(c.runs(instance).await["runs"].as_array().unwrap().len(), 1);

    // Run now; a non-zero exit fails the run.
    rt.once_exit
        .lock()
        .unwrap()
        .insert("node nightly.js".into(), 3);
    let r = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/cron-runs"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["trigger"], "manual");
    let failed = c
        .wait_run(instance, r.json["id"].as_str().unwrap(), "failed")
        .await;
    assert_eq!(failed["exitCode"], 3);
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'cron.run'")
            .fetch_one(c.app.db())
            .await
            .unwrap();
    assert_eq!(audited, 1);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_due_time_is_claimed_once(pool: PgPool) {
    let c = cluster(pool).await;
    c.start_node().await;
    let (_, instance) = c.service(job(&["true"])).await;
    c.deploy_active(instance).await;
    cron::tick(&c.app.state).await.unwrap();
    c.make_due(instance).await;
    // Two server replicas ticking at once: only one wins the due time.
    let now = OffsetDateTime::now_utc();
    let (a, b) = tokio::join!(
        cron::claim_due(&c.app.state, now),
        cron::claim_due(&c.app.state, now)
    );
    let claimed = a.unwrap().len() + b.unwrap().len();
    assert_eq!(claimed, 1);
    assert!(cron::claim_due(&c.app.state, now).await.unwrap().is_empty());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn overlapping_runs_are_skipped_and_long_ones_time_out(pool: PgPool) {
    let c = cluster(pool).await;
    let rt = c.start_node().await;
    let mut cfg = job(&["sleep", "60"]);
    cfg["cronTimeoutSecs"] = json!(1);
    let (_, instance) = c.service(cfg).await;
    c.deploy_active(instance).await;
    rt.once_delay
        .lock()
        .unwrap()
        .insert("sleep 60".into(), Duration::from_secs(60));

    let r = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/cron-runs"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "running");
    let long = r.json["id"].as_str().unwrap().to_owned();

    // A second manual run is refused; a due time is recorded as skipped.
    let again = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/cron-runs"),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    cron::tick(&c.app.state).await.unwrap();
    c.make_due(instance).await;
    cron::tick(&c.app.state).await.unwrap();
    let runs = c.runs(instance).await;
    let skipped = &runs["runs"][0];
    assert_eq!(skipped["status"], "skipped", "{runs}");
    assert_eq!(skipped["trigger"], "schedule");

    // The node stops it at the timeout.
    let timed_out = c.wait_run(instance, &long, "timed_out").await;
    assert!(
        timed_out["error"].as_str().unwrap().contains("1s"),
        "{timed_out}"
    );
    assert!(timed_out["exitCode"].is_null());

    // A run whose result never comes is given up on by the server.
    let lost = Id::new();
    sqlx::query(
        "INSERT INTO cron_runs (id, service_instance_id, trigger, status, timeout_secs, started_at)
         VALUES ($1, $2, 'manual', 'running', 1, now() - interval '1 hour')",
    )
    .bind(lost)
    .bind(instance)
    .execute(c.app.db())
    .await
    .unwrap();
    cron::tick(&c.app.state).await.unwrap();
    let r = c.wait_run(instance, &lost.to_string(), "timed_out").await;
    assert_eq!(r["error"], "no result from the server running it");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn schedules_are_validated(pool: PgPool) {
    let c = cluster(pool).await;
    let (_, instance) = c.service(job(&["true"])).await;
    let base = job(&["true"]);
    for bad in [
        json!({ "cronSchedule": "every night" }),
        json!({ "cronSchedule": "0 3 * * * *" }),
        json!({ "cronSchedule": "0 25 * * *" }),
        json!({ "cronSchedule": "0 3 * * *", "replicas": 2 }),
        json!({ "cronSchedule": "0 3 * * *", "cronTimeoutSecs": 0 }),
        json!({ "cronSchedule": "0 3 * * *", "cronTimeoutSecs": 86401 }),
    ] {
        let mut cfg = base.clone();
        cfg.as_object_mut()
            .unwrap()
            .extend(bad.as_object().unwrap().clone());
        let r = c
            .call(
                Method::PATCH,
                &format!("/api/service-instances/{instance}"),
                Some(json!({ "config": cfg })),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}: {:?}", r.json);
    }
    let mut no_schedule = base.clone();
    no_schedule.as_object_mut().unwrap().remove("cronSchedule");
    no_schedule["cronTimeoutSecs"] = json!(60);
    let r = c
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}"),
            Some(json!({ "config": no_schedule })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    // The preview explains an expression or says what is wrong with it.
    let r = c
        .call(
            Method::GET,
            "/api/cron/preview?schedule=*/15%20*%20*%20*%20*",
            None,
        )
        .await;
    assert_eq!(r.json["valid"], true);
    assert_eq!(r.json["description"], "every 15 minutes");
    assert_eq!(r.json["upcoming"].as_array().unwrap().len(), 3);
    let r = c
        .call(Method::GET, "/api/cron/preview?schedule=nope", None)
        .await;
    assert_eq!(r.json["valid"], false);
    assert!(r.json["error"].as_str().unwrap().contains("5 fields"));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn viewers_see_runs_and_deployers_start_them(pool: PgPool) {
    let c = cluster(pool).await;
    c.start_node().await;
    let (ws, instance) = c.service(job(&["true"])).await;
    c.deploy_active(instance).await;
    let path = format!("/api/service-instances/{instance}/cron-runs");
    let (viewer_id, viewer) = c.app.user("viewer@example.com").await;
    c.app.add_member(ws, viewer_id, "viewer").await;
    let (deployer_id, deployer) = c.app.user("deployer@example.com").await;
    c.app.add_member(ws, deployer_id, "deployer").await;

    let r = c.app.call(Method::GET, &path, Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = c.app.call(Method::POST, &path, Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = c.app.call(Method::POST, &path, Some(&deployer), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // Outside the workspace it doesn't exist.
    let (_, stranger) = c.app.user("stranger@example.com").await;
    let r = c.app.call(Method::GET, &path, Some(&stranger), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}
