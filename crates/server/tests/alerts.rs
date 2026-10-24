//! Alerts: notification channels (SSRF-guarded, sealed URLs, test sends),
//! rules, event alerts from deployment failures, and the minute evaluation
//! of metric, disk and offline rules with cooldown and "resolved". Every
//! delivery goes to a local mock receiver.

mod common;

use std::sync::{Arc, Mutex};

use axum::http::{HeaderMap, Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_proto::node::v1 as pb;
use modula_server::alerts::{self, eval};
use modula_server::jobs::Job;
use serde_json::{Value, json};
use sqlx::PgPool;

#[derive(Clone, Default)]
struct Received(Arc<Mutex<Vec<Request>>>);

/// Headers and body of one request the receiver got.
type Request = (HeaderMap, Vec<u8>);

impl Received {
    fn bodies(&self) -> Vec<Value> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| serde_json::from_slice(b).unwrap())
            .collect()
    }
}

/// A receiver on loopback (which the delivery guard is told to allow).
async fn mock_receiver() -> (String, Received) {
    modula_server::webhooks::allow_loopback_for_tests(true);
    let got = Received::default();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post({
            let got = got.clone();
            move |headers: HeaderMap, body: axum::body::Bytes| async move {
                got.0.lock().unwrap().push((headers, body.to_vec()));
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{}/hook", addr.port()), got)
}

/// A channel pointing at `url`, inserted directly (the API refuses loopback).
async fn channel(app: &TestApp, ws: Id, kind: &str, url: &str, secret: Option<&str>) -> Id {
    let id = Id::new();
    let row = id.to_string();
    let keys = &app.state.keys;
    let url_sealed = keys
        .seal(app.db(), url.as_bytes(), alerts::url_aad(ws, &row))
        .await
        .unwrap();
    let secret_sealed = match secret {
        Some(s) => Some(
            keys.seal(app.db(), s.as_bytes(), alerts::secret_aad(ws, &row))
                .await
                .unwrap(),
        ),
        None => None,
    };
    sqlx::query(
        "INSERT INTO notification_channels (id, workspace_id, kind, name, host, url_sealed, secret_sealed)
         VALUES ($1, $2, $3, 'ops', '127.0.0.1', $4, $5)",
    )
    .bind(id)
    .bind(ws)
    .bind(kind)
    .bind(url_sealed)
    .bind(secret_sealed)
    .execute(app.db())
    .await
    .unwrap();
    id
}

async fn rule(app: &TestApp, admin: &str, ws: Id, body: Value) -> Value {
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/alert-rules"),
            Some(admin),
            Some(body),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    r.json
}

fn job(kind: &str, payload: Value) -> Job {
    Job {
        id: 0,
        kind: kind.into(),
        payload,
        attempts: 1,
        max_attempts: 1,
    }
}

async fn eval_rule(app: &TestApp, id: &str) {
    let r: eval::Rule = sqlx::query_as(
        "SELECT id, workspace_id, name, kind, project_id, service_id, threshold, window_minutes,
                cooldown_minutes FROM alert_rules WHERE id = $1",
    )
    .bind(id)
    .fetch_one(app.db())
    .await
    .unwrap();
    eval::evaluate_rule(&app.state, &r).await.unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn channels_are_guarded_sealed_and_testable(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let base = format!("/api/workspaces/{ws}/notification-channels");

    for url in [
        "http://127.0.0.1:9000/hook",
        "http://169.254.169.254/latest/meta-data",
        "http://10.0.0.1/hook",
        "http://localhost/hook",
        "http://[::1]/hook",
        "ftp://hooks.slack.com/x",
    ] {
        let r = app
            .call(
                Method::POST,
                &base,
                Some(&admin),
                Some(json!({"kind": "slack", "name": "ops", "url": url})),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{url}: {}", r.json);
    }
    let r = app
        .call(
            Method::POST,
            &base,
            Some(&admin),
            Some(json!({"kind": "email", "name": "ops", "url": "https://hooks.slack.com/services/T/B/x"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Slack and Discord hosts pass the ordinary public-host check.
    for (kind, url) in [
        (
            "slack",
            "https://hooks.slack.com/services/T000/B000/secrettoken",
        ),
        ("discord", "https://discord.com/api/webhooks/1/secrettoken"),
    ] {
        let r = app
            .call(
                Method::POST,
                &base,
                Some(&admin),
                Some(json!({"kind": kind, "name": kind, "url": url})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        assert!(r.json.get("secret").is_none());
        assert!(!r.json.to_string().contains("secrettoken"));
    }
    let r = app
        .call(
            Method::POST,
            &base,
            Some(&admin),
            Some(json!({"kind": "webhook", "name": "pager", "url": "https://example.com/hook"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(r.json["secret"].as_str().unwrap().starts_with("whsec_"));
    assert_eq!(r.json["host"], "example.com");

    let r = app.call(Method::GET, &base, Some(&admin), None).await;
    assert_eq!(r.json.as_array().unwrap().len(), 3);
    assert!(!r.json.to_string().contains("secrettoken"));
    // Sealed at rest.
    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT url_sealed FROM notification_channels")
        .fetch_all(app.db())
        .await
        .unwrap();
    for s in stored {
        assert!(!String::from_utf8_lossy(&s).contains("secrettoken"));
    }

    // "Send test" to a receiver: Slack's format, recorded in the history.
    let (url, got) = mock_receiver().await;
    let slack = channel(&app, ws, "slack", &url, None).await;
    let r = app
        .call(
            Method::POST,
            &format!("/api/notification-channels/{slack}/test"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["status"], 200);
    let bodies = got.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["text"], "Test notification from Modula");
    assert_eq!(bodies[0]["blocks"][0]["type"], "header");
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/alert-history"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json[0]["phase"], "test");
    assert_eq!(r.json[0]["status"], 200);
    assert_eq!(r.json[0]["channelKind"], "slack");

    // Generic webhooks are signed with the channel's secret.
    let hook = channel(&app, ws, "webhook", &url, Some("whsec_test")).await;
    let r = app
        .call(
            Method::POST,
            &format!("/api/notification-channels/{hook}/test"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["status"], 200, "{}", r.json);
    let (headers, body) = got.0.lock().unwrap()[1].clone();
    assert_eq!(headers["x-modula-event"], "test");
    assert_eq!(
        headers["x-modula-signature"].to_str().unwrap(),
        modula_server::webhooks::sign(b"whsec_test", &body)
    );

    // A channel stored with an internal literal address is refused at send time too.
    let internal = channel(&app, ws, "slack", "http://169.254.169.254/x", None).await;
    let r = app
        .call(
            Method::POST,
            &format!("/api/notification-channels/{internal}/test"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["status"], Value::Null);
    assert!(r.json["error"].as_str().unwrap().contains("refused"));

    // Viewers can't manage channels.
    let (viewer_id, viewer) = app.user("viewer@example.com").await;
    app.add_member(ws, viewer_id, "viewer").await;
    let r = app.call(Method::GET, &base, Some(&viewer), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn rules_are_validated_and_scoped(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let other = app.create_workspace(&admin, "other").await;
    let chain = app.seed_chain(ws).await;
    let foreign = app.seed_chain(other).await;
    let ch = channel(&app, ws, "slack", "https://hooks.slack.com/x", None).await;
    let foreign_ch = channel(&app, other, "slack", "https://hooks.slack.com/y", None).await;
    let base = format!("/api/workspaces/{ws}/alert-rules");
    for (body, why) in [
        (
            json!({"name": "a", "kind": "cpu", "channelIds": [ch]}),
            "no threshold",
        ),
        (
            json!({"name": "a", "kind": "cpu", "threshold": 80, "windowMinutes": 90, "channelIds": [ch]}),
            "window",
        ),
        (
            json!({"name": "a", "kind": "deployment_failed", "channelIds": []}),
            "no channel",
        ),
        (
            json!({"name": "a", "kind": "deployment_failed", "channelIds": [foreign_ch]}),
            "foreign channel",
        ),
        (
            json!({"name": "a", "kind": "deployment_failed", "projectId": foreign.project, "channelIds": [ch]}),
            "foreign project",
        ),
        (
            json!({"name": "a", "kind": "deployment_failed", "serviceId": chain.service, "channelIds": [ch]}),
            "service without project",
        ),
        (
            json!({"name": "a", "kind": "sms", "channelIds": [ch]}),
            "kind",
        ),
    ] {
        let r = app
            .call(Method::POST, &base, Some(&admin), Some(body))
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{why}: {}", r.json);
    }
    let wide = rule(
        &app,
        &admin,
        ws,
        json!({"name": "All CPU", "kind": "cpu", "threshold": 80, "windowMinutes": 5, "channelIds": [ch, ch]}),
    )
    .await;
    assert_eq!(wide["channelIds"], json!([ch]));
    assert_eq!(wide["cooldownMinutes"], 30);
    let scoped = rule(
        &app,
        &admin,
        ws,
        json!({"name": "Web deploys", "kind": "deployment_failed", "projectId": chain.project,
               "serviceId": chain.service, "threshold": 50, "channelIds": [ch]}),
    )
    .await;
    assert_eq!(scoped["threshold"], Value::Null);
    assert_eq!(scoped["serviceName"], "web");

    let r = app
        .call(
            Method::GET,
            &format!("/api/projects/{}/alert-rules", chain.project),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 2);

    let id = scoped["id"].as_str().unwrap();
    let r = app
        .call(
            Method::PATCH,
            &format!("/api/alert-rules/{id}"),
            Some(&admin),
            Some(json!({"enabled": false, "cooldownMinutes": 5})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert_eq!(r.json["enabled"], false);
    assert_eq!(r.json["cooldownMinutes"], 5);
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/alert-rules/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_failed_deployment_queues_an_alert_and_notifies_once_per_cooldown(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;

    for (status, kind) in [
        ("deploying", "deployment_failed"),
        ("building", "build_failed"),
    ] {
        sqlx::query("UPDATE deployments SET status = $2 WHERE id = $1")
            .bind(chain.deployment)
            .bind(status)
            .execute(app.db())
            .await
            .unwrap();
        let ev = pb::DeploymentEvent {
            deployment_id: chain.deployment.to_string(),
            phase: pb::DeploymentPhase::Failed as i32,
            message: "health check timed out".into(),
            ..Default::default()
        };
        modula_server::deploy::state::apply_event(app.db(), chain.node, &ev)
            .await
            .unwrap();
        let payload: Value = sqlx::query_scalar(
            "SELECT payload FROM jobs WHERE kind = 'alerts.event' ORDER BY id DESC LIMIT 1",
        )
        .fetch_one(app.db())
        .await
        .unwrap();
        assert_eq!(payload["kind"], kind);
        assert_eq!(payload["serviceInstanceId"], chain.instance.to_string());
        assert_eq!(payload["deploymentId"], chain.deployment.to_string());
    }

    let (url, got) = mock_receiver().await;
    let ch = channel(&app, ws, "discord", &url, None).await;
    rule(
        &app,
        &admin,
        ws,
        json!({"name": "Deploys", "kind": "build_failed", "projectId": chain.project, "channelIds": [ch]}),
    )
    .await;
    // A rule of another kind stays quiet.
    rule(
        &app,
        &admin,
        ws,
        json!({"name": "Crashes", "kind": "deployment_crashed", "channelIds": [ch]}),
    )
    .await;
    let payload = json!({
        "kind": "build_failed",
        "serviceInstanceId": chain.instance,
        "deploymentId": chain.deployment,
    });
    eval::event_job(app.state.clone(), job(alerts::EVENT_JOB, payload.clone()))
        .await
        .unwrap();
    let bodies = got.bodies();
    assert_eq!(bodies.len(), 1);
    let e = &bodies[0]["embeds"][0];
    assert_eq!(e["title"], "web failed to build");
    assert_eq!(e["description"], "health check timed out");
    assert_eq!(e["fields"][0]["value"], "p");
    assert_eq!(e["fields"][1]["value"], "production");
    let link = e["url"].as_str().unwrap();
    assert!(
        link.contains(&format!(
            "/projects/{}?env={}&service={}&tab=deployments&deployment={}",
            chain.project, chain.environment, chain.service, chain.deployment
        )),
        "{link}"
    );
    // Within the cooldown: not sent again.
    eval::event_job(app.state.clone(), job(alerts::EVENT_JOB, payload))
        .await
        .unwrap();
    assert_eq!(got.bodies().len(), 1);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn metric_rules_fire_once_and_resolve(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    sqlx::query(
        r#"UPDATE deployments SET config = '{"cpuMillis": 500, "memoryMb": 100}' WHERE id = $1"#,
    )
    .bind(chain.deployment)
    .execute(app.db())
    .await
    .unwrap();
    let samples = |cpu: f32, mem: i64| {
        let db = app.db().clone();
        let (dep, inst) = (chain.deployment, chain.instance);
        async move {
            sqlx::query("DELETE FROM metric_samples")
                .execute(&db)
                .await
                .unwrap();
            // Two samples a minute over the last 7 complete minutes.
            sqlx::query(
                "INSERT INTO metric_samples (deployment_id, service_instance_id, at, cpu_percent,
                                             memory_bytes, net_rx_bps, net_tx_bps)
                 SELECT $1, $2, date_trunc('minute', now()) - make_interval(secs => s * 30 + 5),
                        $3, $4, 0, 0
                 FROM generate_series(0, 13) s",
            )
            .bind(dep)
            .bind(inst)
            .bind(cpu)
            .bind(mem)
            .execute(&db)
            .await
            .unwrap();
        }
    };
    let (url, got) = mock_receiver().await;
    let ch = channel(&app, ws, "webhook", &url, Some("whsec_x")).await;
    let cpu = rule(
        &app,
        &admin,
        ws,
        json!({"name": "CPU", "kind": "cpu", "threshold": 80, "windowMinutes": 5, "channelIds": [ch]}),
    )
    .await;
    let mem = rule(
        &app,
        &admin,
        ws,
        json!({"name": "Memory", "kind": "memory", "threshold": 90, "windowMinutes": 5, "channelIds": [ch]}),
    )
    .await;
    let (cpu, mem) = (cpu["id"].as_str().unwrap(), mem["id"].as_str().unwrap());

    // 45% of one core is 90% of a half-core limit; 50 MB of 100 MB is 50%.
    samples(45.0, 50 * 1024 * 1024).await;
    eval_rule(&app, cpu).await;
    eval_rule(&app, mem).await;
    let bodies = got.bodies();
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert_eq!(bodies[0]["type"], "alert.fired");
    assert_eq!(bodies[0]["title"], "High CPU: web");
    assert_eq!(bodies[0]["rule"]["kind"], "cpu");
    assert!(
        bodies[0]["reason"].as_str().unwrap().contains("90%"),
        "{}",
        bodies[0]
    );
    assert_eq!(bodies[0]["subject"], format!("instance:{}", chain.instance));
    // Still high: nothing new.
    eval_rule(&app, cpu).await;
    assert_eq!(got.bodies().len(), 1);
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/alert-rules"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json[0]["firing"], 1);

    // Back down: resolved, once.
    samples(10.0, 50 * 1024 * 1024).await;
    eval_rule(&app, cpu).await;
    eval_rule(&app, cpu).await;
    let bodies = got.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[1]["type"], "alert.resolved");
    assert_eq!(bodies[1]["title"], "CPU back to normal: web");

    // High again within the 30-minute cooldown: not sent.
    samples(45.0, 99 * 1024 * 1024).await;
    eval_rule(&app, cpu).await;
    assert_eq!(got.bodies().len(), 2);
    // Memory above 90% of its limit fires on its own rule.
    eval_rule(&app, mem).await;
    let bodies = got.bodies();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[2]["title"], "High memory: web");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn offline_servers_and_full_disks_alert(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let chain = app.seed_chain(ws).await;
    let (url, got) = mock_receiver().await;
    let ch = channel(&app, ws, "slack", &url, None).await;
    let offline = rule(
        &app,
        &admin,
        ws,
        json!({"name": "Offline", "kind": "node_offline", "windowMinutes": 5, "channelIds": [ch]}),
    )
    .await;
    let disk = rule(
        &app,
        &admin,
        ws,
        json!({"name": "Disk", "kind": "disk", "threshold": 90, "channelIds": [ch]}),
    )
    .await;
    let (offline, disk) = (
        offline["id"].as_str().unwrap(),
        disk["id"].as_str().unwrap(),
    );

    // Seen a minute ago: fine.
    sqlx::query("UPDATE nodes SET status = 'online', last_seen_at = now() - interval '1 minute'")
        .execute(app.db())
        .await
        .unwrap();
    eval_rule(&app, offline).await;
    assert!(got.bodies().is_empty());
    sqlx::query(
        "UPDATE nodes SET status = 'offline', last_seen_at = now() - interval '10 minutes'",
    )
    .execute(app.db())
    .await
    .unwrap();
    eval_rule(&app, offline).await;
    let bodies = got.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["text"], "Server offline: n");
    sqlx::query("UPDATE nodes SET status = 'online', last_seen_at = now()")
        .execute(app.db())
        .await
        .unwrap();
    eval_rule(&app, offline).await;
    assert_eq!(got.bodies()[1]["text"], "Resolved: Server back online: n");

    // The node's disk report: the fullest filesystem counts.
    let report = pb::DiskReport {
        filesystems: vec![
            pb::FilesystemUsage {
                path: "/var/lib/modula".into(),
                total_bytes: 100,
                free_bytes: 50,
            },
            pb::FilesystemUsage {
                path: "/var/lib/docker".into(),
                total_bytes: 100,
                free_bytes: 5,
            },
        ],
        volumes: vec![pb::VolumeUsage {
            name: format!("modula-vol-{}", chain.volume),
            used_bytes: 4096,
        }],
    };
    modula_server::metrics::record_disk(app.db(), chain.node, &report)
        .await
        .unwrap();
    let used: i64 = sqlx::query_scalar("SELECT used_bytes FROM volume_usage WHERE volume_id = $1")
        .bind(chain.volume)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(used, 4096);
    eval_rule(&app, disk).await;
    let bodies = got.bodies();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[2]["text"], "Disk almost full: n");
    assert!(
        bodies[2]
            .to_string()
            .contains("/var/lib/docker is 95% full"),
        "{}",
        bodies[2]
    );
}
