//! Zero-downtime restarts and updates: in-flight requests finish across a
//! shutdown, the release check trusts only signed versions, upgrades are
//! handed to the upgrade unit (and its report recorded), the maintenance
//! window triggers them, only instance admins in sudo mode change any of it,
//! and other servers are upgraded one at a time over their streams.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{Method, StatusCode};
use axum::routing::get;
use common::*;
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use modula_node::upgrade::FakeUpgrader;
use modula_server::listen;
use modula_server::updates;
use ring::signature::KeyPair;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

fn temp_dir(what: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("modula-{what}-{}", Id::new()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn wait_for<F, Fut>(what: &str, secs: u64, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let until = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < until {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---------------------------------------------------------------- graceful drain

/// A request that is running when shutdown begins still gets its response;
/// the listener stops accepting at once; the drain is bounded.
#[tokio::test]
async fn in_flight_requests_finish_across_shutdown() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let app = Router::new()
        .route(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(800)).await;
                "done"
            }),
        )
        .route(
            "/hang",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                "never"
            }),
        );
    // Adopted the way systemd's sockets are.
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let mut inherited = listen::Inherited::adopt([std_listener]).unwrap();
    let listener = inherited.listener(addr).await.unwrap();

    let stop = CancellationToken::new();
    let server = tokio::spawn(listen::serve_http(
        listener,
        app,
        stop.clone(),
        Duration::from_secs(5),
    ));
    let client = reqwest::Client::new();
    let slow = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .get(format!("http://{addr}/slow"))
                .send()
                .await?
                .text()
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.cancel();
    assert_eq!(
        slow.await.unwrap().unwrap(),
        "done",
        "the in-flight request completes"
    );
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("the server stops once nothing is in flight")
        .unwrap()
        .unwrap();
    assert!(
        reqwest::Client::new()
            .get(format!("http://{addr}/slow"))
            .timeout(Duration::from_secs(1))
            .send()
            .await
            .is_err(),
        "nothing accepts after shutdown (systemd's socket would hold them instead)"
    );

    // A request that never ends doesn't hold shutdown beyond the drain period.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/hang",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            "never"
        }),
    );
    let stop = CancellationToken::new();
    let server = tokio::spawn(listen::serve_http(
        listener,
        app,
        stop.clone(),
        Duration::from_millis(300),
    ));
    let _hang = tokio::spawn(async move { reqwest::get(format!("http://{addr}/hang")).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = Instant::now();
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("bounded drain")
        .unwrap()
        .unwrap();
    assert!(t.elapsed() >= Duration::from_millis(300));
}

// ---------------------------------------------------------------- a release server

struct Release {
    key: ring::signature::Ed25519KeyPair,
    /// file name → contents, served under /latest/download/.
    files: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    url: url::Url,
}

impl Release {
    async fn start() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let files: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>> =
            Arc::default();
        let app = Router::new().route(
            "/releases/latest/download/{file}",
            get({
                let files = files.clone();
                move |axum::extract::Path(file): axum::extract::Path<String>| {
                    let body = files.lock().unwrap().get(&file).cloned();
                    async move {
                        match body {
                            Some(b) => (StatusCode::OK, b),
                            None => (StatusCode::NOT_FOUND, vec![]),
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/releases", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { key, files, url }
    }

    fn public_key(&self) -> [u8; 32] {
        self.key.public_key().as_ref().try_into().unwrap()
    }

    /// Publish `version` as the latest release.
    fn publish(&self, version: &str) {
        let version_file = format!("{version}\n").into_bytes();
        let sums =
            format!("{}  VERSION\n", hex::encode(Sha256::digest(&version_file))).into_bytes();
        let sig = self.key.sign(&sums).as_ref().to_vec();
        let mut files = self.files.lock().unwrap();
        files.insert("VERSION".into(), version_file);
        files.insert("SHA256SUMS".into(), sums);
        files.insert("SHA256SUMS.sig".into(), sig);
    }
}

fn config_with(
    release: &Release,
    upgrade_dir: Option<std::path::PathBuf>,
) -> modula_server::config::Config {
    let mut cfg = config();
    cfg.releases.url = release.url.clone();
    cfg.releases.pubkey = release.public_key();
    cfg.releases.upgrade_dir = upgrade_dir;
    cfg
}

async fn updates_json(app: &TestApp, admin: &str) -> Value {
    let r = app
        .call(Method::GET, "/api/instance/updates", Some(admin), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    r.json
}

// ---------------------------------------------------------------- checks and upgrades

#[sqlx::test]
async fn checks_trust_only_signed_versions(pool: PgPool) {
    let release = Release::start().await;
    let app = TestApp::with_config(pool, config_with(&release, None));
    let admin = app.setup_admin().await;

    // Releases from before update checks have no VERSION: nothing known, no error.
    let r = app
        .call(
            Method::POST,
            "/api/instance/updates/check",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["latestVersion"], Value::Null);
    assert_eq!(r.json["checkError"], Value::Null);
    assert_eq!(r.json["currentVersion"], updates::VERSION);
    assert_eq!(r.json["mode"], "notify");
    assert_eq!(r.json["window"]["days"], json!([0]), "Sunday by default");
    assert_eq!(r.json["window"]["time"], "03:00");

    release.publish("99.0.0");
    let r = app
        .call(
            Method::POST,
            "/api/instance/updates/check",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["latestVersion"], "99.0.0");
    assert_eq!(r.json["updateAvailable"], true);
    assert_eq!(
        r.json["releaseNotesUrl"],
        format!("{}/download/v99.0.0/", release.url)
    );
    assert_eq!(r.json["canUpgrade"], false, "no upgrade unit in tests");
    // Announced once in the audit log.
    for _ in 0..2 {
        app.call(
            Method::POST,
            "/api/instance/updates/check",
            Some(&admin),
            None,
        )
        .await;
    }
    let announced: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'instance.update.available'",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(announced, 1);

    // A tampered VERSION is rejected and the last good answer kept.
    release
        .files
        .lock()
        .unwrap()
        .insert("VERSION".into(), b"100.0.0\n".to_vec());
    let r = app
        .call(
            Method::POST,
            "/api/instance/updates/check",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["latestVersion"], "99.0.0");
    assert!(
        r.json["checkError"]
            .as_str()
            .unwrap()
            .contains("VERSION does not match"),
        "{:?}",
        r.json
    );
}

#[sqlx::test]
async fn update_now_hands_the_version_to_the_upgrade_unit(pool: PgPool) {
    let release = Release::start().await;
    let dir = temp_dir("upgrade-dir");
    let app = TestApp::with_config(pool, config_with(&release, Some(dir.clone())));
    let admin = app.setup_admin().await;

    // Nothing newer: nothing to do.
    let r = app
        .call(
            Method::POST,
            "/api/instance/updates/run",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);

    release.publish("99.0.0");
    app.call(
        Method::POST,
        "/api/instance/updates/check",
        Some(&admin),
        None,
    )
    .await;
    let r = app
        .call(
            Method::POST,
            "/api/instance/updates/run",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        std::fs::read_to_string(dir.join("request")).unwrap(),
        "99.0.0\n"
    );
    assert_eq!(r.json["lastRun"]["status"], "requested");
    assert_eq!(r.json["lastRun"]["trigger"], "manual");
    assert_eq!(r.json["lastRun"]["toVersion"], "99.0.0");
    // One at a time.
    let again = app
        .call(
            Method::POST,
            "/api/instance/updates/run",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT);

    // modula-upgrade takes the request…
    std::fs::remove_file(dir.join("request")).unwrap();
    assert_eq!(
        updates_json(&app, &admin).await["lastRun"]["status"],
        "running"
    );
    // …and reports a rollback, recorded with the log and in the audit log.
    std::fs::write(
        dir.join("result"),
        "status=rolled_back\nfrom=0.1.0\nto=99.0.0\nfinished_at=2026-10-25T03:01:00Z\n\n  ✗ The new version did not start; rolling back to 0.1.0.\n",
    )
    .unwrap();
    updates::reconcile(&app.state).await.unwrap();
    let s = updates_json(&app, &admin).await;
    assert_eq!(s["lastRun"]["status"], "rolled_back");
    assert!(
        s["lastRun"]["log"]
            .as_str()
            .unwrap()
            .contains("rolling back")
    );
    assert!(s["lastRun"]["finishedAt"].is_string());
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action IN ('instance.upgrade.request', 'instance.upgrade.rolled_back')",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(audited, 2);
    // Reading the same report again changes nothing.
    updates::reconcile(&app.state).await.unwrap();
    assert_eq!(
        updates_json(&app, &admin).await["lastRun"]["status"],
        "rolled_back"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[sqlx::test]
async fn automatic_updates_wait_for_the_maintenance_window(pool: PgPool) {
    let release = Release::start().await;
    let dir = temp_dir("upgrade-auto");
    let app = TestApp::with_config(pool, config_with(&release, Some(dir.clone())));
    let admin = app.setup_admin().await;
    release.publish("99.0.0");

    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    let hhmm = |z: &jiff::Zoned| format!("{:02}:{:02}", z.hour(), z.minute());
    let all_days = json!([0, 1, 2, 3, 4, 5, 6]);
    let put = |mode: &str, time: String| {
        let body = json!({ "mode": mode, "channel": "stable",
                           "window": { "days": all_days, "time": time, "timezone": "UTC" } });
        let (app, admin) = (&app, &admin);
        async move {
            let r = app
                .call(
                    Method::PUT,
                    "/api/instance/updates",
                    Some(admin),
                    Some(body),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
            r.json
        }
    };

    // Auto, but the window opens in two hours: nothing yet.
    let later = now
        .checked_add(jiff::SignedDuration::from_hours(2))
        .unwrap();
    let s = put("auto", hhmm(&later)).await;
    assert_eq!(s["window"]["timezone"], "UTC");
    assert!(s["nextWindow"].is_string());
    updates::tick(&app.state).await.unwrap();
    assert!(!dir.join("request").exists());

    // Notify only, inside the window: still nothing.
    let earlier = now
        .checked_sub(jiff::SignedDuration::from_mins(10))
        .unwrap();
    put("notify", hhmm(&earlier)).await;
    updates::tick(&app.state).await.unwrap();
    assert!(!dir.join("request").exists());

    // Auto inside the window: requested, once.
    put("auto", hhmm(&earlier)).await;
    updates::tick(&app.state).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("request")).unwrap(),
        "99.0.0\n"
    );
    std::fs::remove_file(dir.join("request")).unwrap();
    std::fs::write(
        dir.join("result"),
        "status=failed\nto=99.0.0\n\ncannot download\n",
    )
    .unwrap();
    updates::tick(&app.state).await.unwrap();
    // A failed version isn't retried automatically.
    assert!(!dir.join("request").exists());
    let s = updates_json(&app, &admin).await;
    assert_eq!(s["lastRun"]["status"], "failed");
    assert_eq!(s["lastRun"]["trigger"], "auto");
    std::fs::remove_dir_all(dir).unwrap();
}

#[sqlx::test]
async fn only_instance_admins_in_sudo_change_updates(pool: PgPool) {
    let release = Release::start().await;
    let app = TestApp::with_config(pool, config_with(&release, None));
    let admin = app.setup_admin().await;
    let (_, user) = app.user("someone@example.com").await;
    let body = json!({ "mode": "auto", "channel": "stable",
                       "window": { "days": [6], "time": "04:30", "timezone": "Europe/Berlin" } });

    for (method, path, body) in [
        (Method::GET, "/api/instance/updates", None),
        (Method::PUT, "/api/instance/updates", Some(body.clone())),
        (Method::POST, "/api/instance/updates/check", None),
        (Method::POST, "/api/instance/updates/run", None),
    ] {
        let r = app.call(method.clone(), path, Some(&user), body).await;
        assert!(
            matches!(r.status, StatusCode::NOT_FOUND | StatusCode::FORBIDDEN),
            "{method} {path}: {}",
            r.status
        );
    }

    // Admins need a recent re-authentication to change settings or upgrade.
    app.drop_sudo().await;
    assert_eq!(updates_json(&app, &admin).await["mode"], "notify");
    let r = app
        .call(
            Method::PUT,
            "/api/instance/updates",
            Some(&admin),
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.code(), "sudo_required");
    let r = app
        .call(
            Method::POST,
            "/api/instance/updates/run",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.code(), "sudo_required");

    let admin = app.login("admin@example.com").await;
    let r = app
        .call(
            Method::PUT,
            "/api/instance/updates",
            Some(&admin),
            Some(body),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["mode"], "auto");
    assert_eq!(
        r.json["window"],
        json!({ "days": [6], "time": "04:30", "timezone": "Europe/Berlin" })
    );
    for bad in [
        json!({ "mode": "auto", "window": { "days": [], "time": "04:30", "timezone": "UTC" } }),
        json!({ "mode": "auto", "window": { "days": [7], "time": "04:30", "timezone": "UTC" } }),
        json!({ "mode": "auto", "window": { "days": [1], "time": "25:00", "timezone": "UTC" } }),
        json!({ "mode": "auto", "window": { "days": [1], "time": "04:30", "timezone": "Mars/Base" } }),
        json!({ "mode": "sometimes", "window": { "days": [1], "time": "04:30", "timezone": "UTC" } }),
        json!({ "mode": "auto", "channel": "nightly", "window": { "days": [1], "time": "04:30", "timezone": "UTC" } }),
    ] {
        let r = app
            .call(
                Method::PUT,
                "/api/instance/updates",
                Some(&admin),
                Some(bad.clone()),
            )
            .await;
        assert!(r.status.is_client_error(), "{bad}: {}", r.status);
    }
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'instance.updates.update'",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

// ---------------------------------------------------------------- rolling upgrades

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

impl Cluster {
    async fn new(pool: PgPool) -> Self {
        let app = TestApp::with_config(pool, config());
        let admin = app.setup_admin().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        tokio::spawn(modula_server::nodes::serve(
            app.state.clone(),
            listener,
            stop.clone(),
        ));
        tokio::spawn(updates::rollout::run(
            app.state.clone(),
            stop.clone(),
            Duration::from_millis(100),
        ));
        Self {
            app,
            admin,
            server_url: format!("https://localhost:{port}"),
            stop,
        }
    }

    /// A node on `version`; `upgrader` None = a release that can't upgrade on request.
    async fn node(&self, name: &str, upgrader: Option<Arc<FakeUpgrader>>) -> Id {
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
        let dir = temp_dir("node");
        let id = join(JoinArgs {
            server: &self.server_url,
            token: r.json["token"].as_str().unwrap(),
            fingerprint: r.json["fingerprint"].as_str().unwrap(),
            hostname: name.into(),
            dir: &dir,
        })
        .await
        .expect("join");
        let caps = modula_node::reconcile::Capabilities {
            upgrade: upgrader.map(|u| u as Arc<dyn modula_node::upgrade::Upgrader>),
            ..Default::default()
        };
        tokio::spawn(modula_node::session::run(
            dir,
            Arc::new(FakeRuntime::new()),
            caps,
            self.stop.child_token(),
        ));
        id.meta.node_id
    }

    async fn version(&self, node: Id) -> Option<String> {
        sqlx::query_scalar("SELECT node_version FROM nodes WHERE id = $1 AND status = 'online'")
            .bind(node)
            .fetch_optional(self.app.db())
            .await
            .unwrap()
            .flatten()
    }

    async fn server_status(&self, node: Id) -> String {
        let s = updates_json(&self.app, &self.admin).await;
        s["servers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["nodeId"] == node.to_string())
            .map(|s| s["status"].as_str().unwrap().to_owned())
            .unwrap_or_default()
    }
}

#[sqlx::test]
async fn servers_are_upgraded_one_at_a_time(pool: PgPool) {
    let c = Cluster::new(pool).await;
    let (a_up, b_up) = (
        Arc::new(FakeUpgrader::new("0.0.1")),
        Arc::new(FakeUpgrader::new("0.0.1")),
    );
    let a = c.node("worker-a", Some(a_up.clone())).await;
    let b = c.node("worker-b", Some(b_up.clone())).await;
    // An older release: no "upgrade" capability, so it's never asked.
    let old = c.node("worker-old", None).await;

    let v = updates::VERSION;
    wait_for(
        "both servers on the control plane's version",
        30,
        || async {
            c.version(a).await.as_deref() == Some(v) && c.version(b).await.as_deref() == Some(v)
        },
    )
    .await;
    let (a_reqs, b_reqs) = (
        a_up.requests.lock().unwrap().clone(),
        b_up.requests.lock().unwrap().clone(),
    );
    assert_eq!(a_reqs.len(), 1);
    assert_eq!(b_reqs.len(), 1);
    assert_eq!(a_reqs[0].0, v);
    // worker-b was asked only after worker-a came back on the new version:
    // its reconnect alone takes at least the node's 1 s backoff.
    assert!(
        b_reqs[0].1.duration_since(a_reqs[0].1) >= Duration::from_secs(1),
        "worker-b was asked {:?} after worker-a",
        b_reqs[0].1.duration_since(a_reqs[0].1)
    );
    wait_for("both upgrades recorded as done", 10, || async {
        let done: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM node_upgrades WHERE status = 'succeeded' AND to_version = $1",
        )
        .bind(v)
        .fetch_one(c.app.db())
        .await
        .unwrap();
        done == 2
    })
    .await;
    assert_eq!(c.server_status(a).await, "current");
    // A server without the capability is never asked (and is never waited for).
    let asked: i64 = sqlx::query_scalar("SELECT count(*) FROM node_upgrades WHERE node_id = $1")
        .bind(old)
        .fetch_one(c.app.db())
        .await
        .unwrap();
    assert_eq!(asked, 0);
}

#[sqlx::test]
async fn a_failed_server_upgrade_pauses_the_rollout_until_retried(pool: PgPool) {
    let c = Cluster::new(pool).await;
    let a_up = Arc::new(FakeUpgrader::new("0.0.1"));
    *a_up.fail.lock().unwrap() = Some("signature check failed".into());
    let b_up = Arc::new(FakeUpgrader::new("0.0.1"));
    let a = c.node("worker-a", Some(a_up.clone())).await;
    wait_for("worker-a's upgrade to fail", 15, || async {
        c.server_status(a).await == "failed"
    })
    .await;
    let s = updates_json(&c.app, &c.admin).await;
    let row = s["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["nodeId"] == a.to_string())
        .unwrap()
        .clone();
    assert!(
        row["error"]
            .as_str()
            .unwrap()
            .contains("signature check failed"),
        "{row}"
    );

    // A server that joins meanwhile waits.
    let b = c.node("worker-b", Some(b_up.clone())).await;
    wait_for("worker-b online", 10, || async {
        c.version(b).await.is_some()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(b_up.requests.lock().unwrap().is_empty(), "paused");
    assert_eq!(c.server_status(b).await, "behind");

    // "Update now" resumes it.
    *a_up.fail.lock().unwrap() = None;
    let r = c
        .app
        .call(
            Method::POST,
            "/api/instance/updates/run",
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let v = updates::VERSION;
    wait_for("both upgraded after the retry", 30, || async {
        c.version(a).await.as_deref() == Some(v) && c.version(b).await.as_deref() == Some(v)
    })
    .await;
    assert_eq!(a_up.requests.lock().unwrap().len(), 2);
}
