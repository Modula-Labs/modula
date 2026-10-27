//! Private dashboard (Cloudflare Tunnel + Access) against an in-process mock
//! of the Cloudflare API: enabling and disabling (records replaced and
//! restored, Access apps and policies, idempotent re-runs, rollback after a
//! partial failure), Access JWT verification, and the origin gate.

mod common;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, Uri};
use axum::response::IntoResponse;
use common::*;
use futures::future::BoxFuture;
use jsonwebtoken::jwk::{Jwk, JwkSet};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use modula_id::Id;
use modula_proto::node::v1 as pb;
use modula_server::private::jwt::{JwtError, KeySource, Verifier};
use modula_server::private::nodes::Agent;
use modula_server::private::{self, Probe, orchestrate};
use modula_server::state::AppState;
use serde_json::{Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

const TOKEN: &str = "cf-test-token-0123456789abcdefWXYZ";
const ZONE: &str = "cloud.test";
const TEAM: &str = "acme.cloudflareaccess.com";
const ORIGINAL_IP: &str = "203.0.113.10";

// ---------------------------------------------------------------- mock Cloudflare

#[derive(Default)]
struct Cf {
    records: Mutex<Vec<Value>>,
    tunnels: Mutex<Vec<Value>>,
    apps: Mutex<Vec<Value>>,
    service_tokens: Mutex<Vec<Value>>,
    next_id: AtomicU32,
    /// Answer 403 to tunnel calls (the token lacks the permission).
    no_tunnel_permission: AtomicBool,
    /// Fail creating the Access app whose name contains this.
    fail_app: Mutex<Option<String>>,
}

impl Cf {
    fn id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next_id.fetch_add(1, Ordering::SeqCst))
    }
    fn records(&self) -> Vec<Value> {
        self.records.lock().unwrap().clone()
    }
    fn named(&self, name: &str) -> Vec<Value> {
        self.records()
            .into_iter()
            .filter(|r| r["name"] == name)
            .collect()
    }
    fn count(&self, what: &Mutex<Vec<Value>>) -> usize {
        what.lock().unwrap().len()
    }
}

fn ok(result: Value) -> axum::response::Response {
    axum::Json(json!({ "success": true, "errors": [], "result": result })).into_response()
}

fn list(items: Vec<Value>) -> axum::response::Response {
    axum::Json(json!({ "success": true, "errors": [], "result": items,
        "result_info": { "page": 1, "per_page": 50, "total_pages": 1 } }))
    .into_response()
}

fn fail(status: StatusCode, message: &str) -> axum::response::Response {
    (
        status,
        axum::Json(json!({ "success": false, "errors": [{ "code": 1, "message": message }], "result": null })),
    )
        .into_response()
}

async fn handle(
    State(cf): State<Arc<Cf>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {TOKEN}"))
    {
        return fail(StatusCode::UNAUTHORIZED, "Invalid API Token");
    }
    let path = uri.path().trim_start_matches("/client/v4/").to_owned();
    let query: HashMap<String, String> = uri
        .query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let seg: Vec<&str> = path.split('/').collect();
    if seg.get(2) == Some(&"cfd_tunnel") && cf.no_tunnel_permission.load(Ordering::SeqCst) {
        return fail(StatusCode::FORBIDDEN, "Authentication error");
    }
    let zone = json!({ "id": "zid1", "name": ZONE, "status": "active", "paused": false,
                       "account": { "id": "acc1", "name": "Acme" } });
    match (method.as_str(), &seg[..]) {
        ("GET", ["user", "tokens", "verify"]) => ok(json!({ "id": "tok1", "status": "active" })),
        ("GET", ["accounts"]) => list(vec![json!({ "id": "acc1", "name": "Acme" })]),
        ("GET", ["zones"]) => match query.get("name") {
            Some(n) if n != ZONE => list(vec![]),
            _ => list(vec![zone]),
        },
        ("GET", ["zones", _, "dns_records"]) => list(
            cf.records()
                .into_iter()
                .filter(|r| query.get("type").is_none_or(|t| r["type"] == **t))
                .filter(|r| query.get("name").is_none_or(|n| r["name"] == **n))
                .collect(),
        ),
        ("POST", ["zones", _, "dns_records"]) => {
            let mut r = body;
            r["id"] = json!(cf.id("rec"));
            cf.records.lock().unwrap().push(r.clone());
            ok(r)
        }
        ("DELETE", ["zones", _, "dns_records", id]) => {
            let mut recs = cf.records.lock().unwrap();
            let before = recs.len();
            recs.retain(|r| r["id"] != *id);
            if recs.len() == before {
                return fail(StatusCode::NOT_FOUND, "Record does not exist.");
            }
            ok(json!({ "id": id }))
        }
        ("GET", ["accounts", "acc1", "access", "organizations"]) => {
            ok(json!({ "auth_domain": TEAM }))
        }
        ("GET", ["accounts", "acc1", "cfd_tunnel"]) => list(cf.tunnels.lock().unwrap().clone()),
        ("POST", ["accounts", "acc1", "cfd_tunnel"]) => {
            assert_eq!(body["config_src"], "cloudflare");
            let t = json!({ "id": cf.id("tun"), "name": body["name"], "status": "inactive", "connections": [] });
            cf.tunnels.lock().unwrap().push(t.clone());
            ok(t)
        }
        ("GET", ["accounts", "acc1", "cfd_tunnel", id]) => {
            match cf.tunnels.lock().unwrap().iter().find(|t| t["id"] == *id) {
                // cloudflared connected as soon as the main server got its token.
                Some(t) => ok(
                    json!({ "id": id, "status": "healthy", "connections": [{}], "name": t["name"] }),
                ),
                None => fail(StatusCode::NOT_FOUND, "tunnel not found"),
            }
        }
        ("GET", ["accounts", "acc1", "cfd_tunnel", id, "token"]) => {
            ok(json!(format!("tunnel-token-{id}")))
        }
        ("PUT", ["accounts", "acc1", "cfd_tunnel", id, "configurations"]) => {
            let mut tunnels = cf.tunnels.lock().unwrap();
            let Some(t) = tunnels.iter_mut().find(|t| t["id"] == *id) else {
                return fail(StatusCode::NOT_FOUND, "tunnel not found");
            };
            t["config"] = body["config"].clone();
            ok(json!({}))
        }
        ("DELETE", ["accounts", "acc1", "cfd_tunnel", _, "connections"]) => ok(json!({})),
        ("DELETE", ["accounts", "acc1", "cfd_tunnel", id]) => {
            let mut tunnels = cf.tunnels.lock().unwrap();
            let before = tunnels.len();
            tunnels.retain(|t| t["id"] != *id);
            if tunnels.len() == before {
                return fail(StatusCode::NOT_FOUND, "tunnel not found");
            }
            ok(json!({}))
        }
        ("GET", ["accounts", "acc1", "access", "apps"]) => list(cf.apps.lock().unwrap().clone()),
        ("POST", ["accounts", "acc1", "access", "apps"]) => {
            if let Some(f) = cf.fail_app.lock().unwrap().as_deref()
                && body["name"].as_str().unwrap().contains(f)
            {
                return fail(StatusCode::INTERNAL_SERVER_ERROR, "boom");
            }
            let mut a = body;
            a["id"] = json!(cf.id("app"));
            a["aud"] = json!(format!("aud-{}", a["id"].as_str().unwrap()));
            cf.apps.lock().unwrap().push(a.clone());
            ok(a)
        }
        ("DELETE", ["accounts", "acc1", "access", "apps", id]) => {
            let mut apps = cf.apps.lock().unwrap();
            let before = apps.len();
            apps.retain(|a| a["id"] != *id);
            if apps.len() == before {
                return fail(StatusCode::NOT_FOUND, "app not found");
            }
            ok(json!({ "id": id }))
        }
        ("POST", ["accounts", "acc1", "access", "service_tokens"]) => {
            let t = json!({ "id": cf.id("st"), "client_id": "cid.access", "client_secret": "csecret", "name": body["name"] });
            cf.service_tokens.lock().unwrap().push(t.clone());
            ok(t)
        }
        ("DELETE", ["accounts", "acc1", "access", "service_tokens", id]) => {
            cf.service_tokens.lock().unwrap().retain(|t| t["id"] != *id);
            ok(json!({ "id": id }))
        }
        _ => fail(StatusCode::NOT_FOUND, &format!("no route {method} {path}")),
    }
}

async fn mock_cloudflare() -> (url::Url, Arc<Cf>) {
    let cf = Arc::new(Cf::default());
    let app = axum::Router::new().fallback(handle).with_state(cf.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/client/v4").parse().unwrap(), cf)
}

// ---------------------------------------------------------------- fakes

/// Records what nodes were asked to do; always succeeds.
#[derive(Default)]
struct FakeAgent {
    calls: Mutex<Vec<(Id, pb::AccessConfig)>>,
}

impl Agent for FakeAgent {
    fn apply<'a>(
        &'a self,
        _: &'a AppState,
        node: Id,
        cfg: pb::AccessConfig,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.calls.lock().unwrap().push((node, cfg));
        Box::pin(async { Ok(()) })
    }
}

struct FakeProbe(AtomicBool);

impl Probe for FakeProbe {
    fn behind_access<'a>(&'a self, _: &'a str) -> BoxFuture<'a, anyhow::Result<bool>> {
        let v = self.0.load(Ordering::SeqCst);
        Box::pin(async move { Ok(v) })
    }
}

/// A fixed, replaceable key set.
struct Keys(Mutex<Vec<Jwk>>, AtomicU32);

impl KeySource for Keys {
    fn fetch<'a>(&'a self, team: &'a str) -> BoxFuture<'a, anyhow::Result<JwkSet>> {
        assert_eq!(team, TEAM);
        self.1.fetch_add(1, Ordering::SeqCst);
        let keys = self.0.lock().unwrap().clone();
        Box::pin(async move { Ok(JwkSet { keys }) })
    }
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn jwk(k: &str) -> Jwk {
    serde_json::from_str(&fixture(&format!("access-{k}.jwk.json"))).unwrap()
}

/// A token signed with key `k` (`kid = key-<k>`).
fn sign(k: &str, aud: &str, iss: &str, exp_in: i64) -> String {
    let mut h = Header::new(Algorithm::RS256);
    h.kid = Some(format!("key-{k}"));
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let claims = json!({ "aud": [aud], "iss": iss, "exp": now + exp_in, "iat": now, "email": "ana@cloud.test" });
    let key = EncodingKey::from_rsa_pem(fixture(&format!("access-{k}.pem")).as_bytes()).unwrap();
    jsonwebtoken::encode(&h, &claims, &key).unwrap()
}

// ---------------------------------------------------------------- setup

struct Env {
    app: TestApp,
    cf: Arc<Cf>,
    agent: Arc<FakeAgent>,
    probe: Arc<FakeProbe>,
    admin: String,
    ws: Id,
    main: Id,
    worker: Id,
}

async fn node(db: &PgPool, name: &str, caps: &[&str]) -> Id {
    let id = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at, info)
         VALUES ($1, $2, 'online', $3, now() + interval '30 days', $4)",
    )
    .bind(id)
    .bind(name)
    .bind(id.to_string())
    .bind(json!({ "capabilities": caps }))
    .execute(db)
    .await
    .unwrap();
    id
}

async fn env(pool: PgPool) -> Env {
    let (base, cf) = mock_cloudflare().await;
    let mut cfg = config();
    cfg.cloudflare_api = base;
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let r = app
        .call(
            Method::PUT,
            &format!("/api/workspaces/{ws}/integrations/cloudflare"),
            Some(&admin),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // The dashboard's own record, as the user created it.
    cf.records.lock().unwrap().push(json!({
        "id": "user-a", "type": "A", "name": ZONE, "content": ORIGINAL_IP, "proxied": false, "ttl": 1
    }));
    let main = node(app.db(), "main", &["edge", "cf-access", "dashboard"]).await;
    let worker = node(app.db(), "worker", &["edge", "cf-access"]).await;
    let agent = Arc::new(FakeAgent::default());
    let probe = Arc::new(FakeProbe(AtomicBool::new(true)));
    let keys = Arc::new(Keys(Mutex::new(vec![jwk("a")]), AtomicU32::new(0)));
    app.state.private.set_hooks(|h| {
        h.agent = agent.clone();
        h.probe = probe.clone();
        h.keys = keys.clone();
        h.wait = Duration::from_millis(100);
        h.poll = Duration::from_millis(10);
    });
    Env {
        app,
        cf,
        agent,
        probe,
        admin,
        ws,
        main,
        worker,
    }
}

async fn plan(e: &Env) -> orchestrate::Plan {
    orchestrate::preflight(
        &e.app.state,
        e.ws,
        None,
        &["ana@cloud.test".into()],
        &["cloud.test".into()],
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------- orchestration

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn enabling_and_disabling_replaces_and_restores_everything(pool: PgPool) {
    let e = env(pool).await;
    let state = &e.app.state;
    let p = plan(&e).await;
    assert_eq!(p.node_host, "modula-nodes.cloud.test");
    assert_eq!(p.main_node, e.main);
    assert_eq!(p.workers, [e.worker]);

    orchestrate::enable(state, p, None).await.unwrap();
    let s = private::settings(&state.db).await.unwrap();
    assert_eq!(s.phase, "on");
    assert!(state.private.is_on());
    assert!(
        !state
            .private
            .node_peer_allowed(Some("198.51.100.1:5000".parse().unwrap()))
    );
    assert!(
        state
            .private
            .node_peer_allowed(Some("127.0.0.1:5000".parse().unwrap()))
    );

    // The user's A record is gone, kept for later; a proxied CNAME replaces it.
    let tunnel = e.cf.tunnels.lock().unwrap()[0].clone();
    let tid = tunnel["id"].as_str().unwrap();
    let dash = e.cf.named(ZONE);
    assert_eq!(dash.len(), 1, "{dash:?}");
    assert_eq!(dash[0]["type"], "CNAME");
    assert_eq!(dash[0]["content"], format!("{tid}.cfargotunnel.com"));
    assert_eq!(dash[0]["proxied"], true);
    assert_eq!(s.previous_dns.unwrap()[0]["content"], ORIGINAL_IP);
    assert_eq!(e.cf.named("modula-nodes.cloud.test")[0]["proxied"], true);

    // Ingress: dashboard → HTTP API, node host → node port, then 404.
    let ingress = &tunnel["config"]["ingress"];
    assert_eq!(ingress[0]["hostname"], ZONE);
    assert!(
        ingress[0]["service"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:")
    );
    assert_eq!(ingress[1]["hostname"], "modula-nodes.cloud.test");
    assert!(
        ingress[1]["service"]
            .as_str()
            .unwrap()
            .starts_with("tcp://127.0.0.1:")
    );
    assert_eq!(ingress[2]["service"], "http_status:404");

    // Access: people + service tokens on the dashboard, a bypass for the
    // self-authenticating endpoints, and only Modula's token for the servers.
    let apps = e.cf.apps.lock().unwrap().clone();
    assert_eq!(apps.len(), 3);
    let dash_app = apps.iter().find(|a| a["domain"] == ZONE).unwrap();
    assert_eq!(dash_app["policies"][0]["decision"], "allow");
    assert_eq!(
        dash_app["policies"][0]["include"][0]["email"]["email"],
        "ana@cloud.test"
    );
    assert_eq!(
        dash_app["policies"][0]["include"][1]["email_domain"]["domain"],
        "cloud.test"
    );
    assert_eq!(dash_app["policies"][1]["decision"], "non_identity");
    let bypass = apps
        .iter()
        .find(|a| a["policies"][0]["decision"] == "bypass")
        .unwrap();
    let uris: Vec<&str> = bypass["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"cloud.test/api/install.sh"), "{uris:?}");
    assert!(uris.contains(&"cloud.test/api/github/webhook"), "{uris:?}");
    let node_app = apps
        .iter()
        .find(|a| a["domain"] == "modula-nodes.cloud.test")
        .unwrap();
    let st = e.cf.service_tokens.lock().unwrap()[0]["id"].clone();
    assert_eq!(
        node_app["policies"][0]["include"][0]["service_token"]["token_id"],
        st
    );
    assert_eq!(
        s.dashboard_aud.as_deref(),
        Some(dash_app["aud"].as_str().unwrap())
    );

    // Nodes: main starts cloudflared, the worker moves, then main hides the dashboard.
    let calls = e.agent.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0].0, e.main);
    assert_eq!(calls[0].1.tunnel_token, format!("tunnel-token-{tid}"));
    assert!(calls[0].1.hidden_routes.is_empty());
    assert_eq!(calls[1].0, e.worker);
    assert_eq!(calls[1].1.node_host, "modula-nodes.cloud.test");
    assert_eq!(calls[1].1.client_secret, "csecret");
    assert!(calls[1].1.tunneled_server.starts_with("https://127.0.0.1:"));
    assert_eq!(calls[2].1.hidden_routes, [ZONE]);
    assert!(calls[2].1.close_node_port);

    // New servers get the tunnel in their join command.
    let access = private::api::join_access(state).await.unwrap().unwrap();
    let cmd = access.apply(
        "x --server https://localhost:7101 --token t",
        "https://localhost:7101",
    );
    assert!(cmd.contains("--server https://127.0.0.1:"), "{cmd}");
    assert!(cmd.ends_with(
        "--access-host modula-nodes.cloud.test --access-id cid.access --access-secret csecret"
    ));

    // Off again: everything restored or removed.
    e.agent.calls.lock().unwrap().clear();
    let warnings = orchestrate::teardown(state, "test").await.unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
    let s = private::settings(&state.db).await.unwrap();
    assert_eq!(s.phase, "off");
    assert!(!state.private.is_on());
    let dash = e.cf.named(ZONE);
    assert_eq!(dash.len(), 1);
    assert_eq!(
        (dash[0]["type"].as_str(), dash[0]["content"].as_str()),
        (Some("A"), Some(ORIGINAL_IP))
    );
    assert!(e.cf.named("modula-nodes.cloud.test").is_empty());
    assert_eq!(e.cf.count(&e.cf.apps), 0);
    assert_eq!(e.cf.count(&e.cf.tunnels), 0);
    assert_eq!(e.cf.count(&e.cf.service_tokens), 0);
    let calls = e.agent.calls.lock().unwrap().clone();
    // Main serves the dashboard again (tunnel still up), worker goes direct, main stops cloudflared.
    assert!(
        calls[0].1.main
            && calls[0].1.hidden_routes.is_empty()
            && !calls[0].1.tunnel_token.is_empty()
    );
    assert_eq!(calls[1].0, e.worker);
    assert!(calls[1].1.node_host.is_empty() && calls[1].1.direct_server.starts_with("https://"));
    assert!(calls[2].1.main && calls[2].1.tunnel_token.is_empty());
    assert!(s.tunnel_id.is_none() && s.dashboard_app_id.is_none() && s.node_token_id.is_none());

    // And a second teardown is harmless.
    assert!(
        orchestrate::teardown(state, "again")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_failed_check_rolls_back_and_a_rerun_creates_no_duplicates(pool: PgPool) {
    let e = env(pool).await;
    let state = &e.app.state;
    // The dashboard never shows the Access sign-in.
    e.probe.0.store(false, Ordering::SeqCst);
    let err = orchestrate::enable(state, plan(&e).await, None)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("Cloudflare Access sign-in"),
        "{err}"
    );
    let s = private::settings(&state.db).await.unwrap();
    assert_eq!(s.phase, "off");
    assert!(s.error.unwrap().contains("Cloudflare Access sign-in"));
    assert!(!state.private.is_on());
    // DNS is back as it was; nothing is left at Cloudflare.
    assert_eq!(e.cf.named(ZONE)[0]["content"], ORIGINAL_IP);
    assert_eq!(e.cf.records().len(), 1);
    assert_eq!(e.cf.count(&e.cf.apps), 0);
    assert_eq!(e.cf.count(&e.cf.tunnels), 0);
    assert_eq!(e.cf.count(&e.cf.service_tokens), 0);

    // Second try works, with exactly one of everything.
    e.probe.0.store(true, Ordering::SeqCst);
    orchestrate::enable(state, plan(&e).await, None)
        .await
        .unwrap();
    assert_eq!(e.cf.count(&e.cf.apps), 3);
    assert_eq!(e.cf.count(&e.cf.tunnels), 1);
    assert_eq!(e.cf.count(&e.cf.service_tokens), 1);
    assert_eq!(e.cf.records().len(), 2);
    // Enabling again is refused while on.
    let again = orchestrate::preflight(state, e.ws, None, &["a@b.co".into()], &[]).await;
    assert!(again.is_err());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_partial_failure_removes_what_was_created(pool: PgPool) {
    let e = env(pool).await;
    let state = &e.app.state;
    *e.cf.fail_app.lock().unwrap() = Some("Modula servers".into());
    let err = orchestrate::enable(state, plan(&e).await, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");
    assert_eq!(
        e.cf.count(&e.cf.apps),
        0,
        "the two apps created first are removed"
    );
    assert_eq!(e.cf.count(&e.cf.tunnels), 0);
    assert_eq!(e.cf.count(&e.cf.service_tokens), 0);
    // DNS was never touched, and no node was asked anything.
    assert_eq!(e.cf.records().len(), 1);
    assert!(e.agent.calls.lock().unwrap().is_empty());
    let s = private::settings(&state.db).await.unwrap();
    assert_eq!(s.phase, "off");
    let steps = s.steps.as_array().unwrap();
    assert!(steps.iter().any(|st| st["status"] == "failed"), "{steps:?}");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn preflight_explains_missing_permissions_and_offline_servers(pool: PgPool) {
    let e = env(pool).await;
    let state = &e.app.state;
    e.cf.no_tunnel_permission.store(true, Ordering::SeqCst);
    let err = orchestrate::preflight(state, e.ws, None, &["a@b.co".into()], &[])
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("Cloudflare Tunnel · Edit"),
        "{err}"
    );
    assert!(err.to_string().contains("replace the token"), "{err}");
    e.cf.no_tunnel_permission.store(false, Ordering::SeqCst);

    let empty = orchestrate::preflight(state, e.ws, None, &[], &[])
        .await
        .unwrap_err();
    assert!(empty.to_string().contains("nobody can sign in"), "{empty}");

    sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = $1")
        .bind(e.worker)
        .execute(&state.db)
        .await
        .unwrap();
    let err = orchestrate::preflight(state, e.ws, None, &["a@b.co".into()], &[])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("worker"), "{err}");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_switch_needs_an_admin_in_sudo(pool: PgPool) {
    let e = env(pool).await;
    let app = &e.app;
    let admin = e.admin.clone();
    let r = app
        .call(
            Method::GET,
            "/api/instance/private-access",
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["phase"], "off");
    let r = app
        .call(
            Method::GET,
            &format!("/api/instance/private-access/check?workspaceId={}", e.ws),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["ready"], true, "{:?}", r.json);
    assert_eq!(r.json["suggestedEmails"][0], "admin@example.com");

    let (_, member) = app.user("someone@example.com").await;
    let r = app
        .call(
            Method::GET,
            "/api/instance/private-access",
            Some(&member),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    app.drop_sudo().await;
    let body =
        json!({ "enabled": true, "workspaceId": e.ws, "allowedEmails": ["admin@example.com"] });
    let r = app
        .call(
            Method::PUT,
            "/api/instance/private-access",
            Some(&admin),
            Some(body),
        )
        .await;
    assert_eq!(r.code(), "sudo_required", "{:?}", r.json);
}

// ---------------------------------------------------------------- JWT

#[tokio::test]
async fn access_tokens_are_verified() {
    let keys = Arc::new(Keys(Mutex::new(vec![jwk("a")]), AtomicU32::new(0)));
    let v = Verifier::new(TEAM, "aud1", keys.clone());
    let iss = format!("https://{TEAM}");

    let claims = v.verify(&sign("a", "aud1", &iss, 300)).await.unwrap();
    assert_eq!(claims.email.as_deref(), Some("ana@cloud.test"));
    assert!(matches!(
        v.verify(&sign("a", "aud1", &iss, -600)).await,
        Err(JwtError::Invalid(_))
    ));
    assert!(matches!(
        v.verify(&sign("a", "other", &iss, 300)).await,
        Err(JwtError::Invalid(_))
    ));
    assert!(matches!(
        v.verify(&sign("a", "aud1", "https://evil.cloudflareaccess.com", 300))
            .await,
        Err(JwtError::Invalid(_))
    ));
    assert_eq!(v.verify("").await.unwrap_err(), JwtError::Missing);
    assert_eq!(v.verify("garbage").await.unwrap_err(), JwtError::Malformed);

    // Signed by key b but claiming key a's id: the signature doesn't verify.
    let forged = {
        let t = sign("b", "aud1", &iss, 300);
        let mut parts: Vec<String> = t.split('.').map(str::to_owned).collect();
        let a = sign("a", "aud1", &iss, 300);
        parts[0] = a.split('.').next().unwrap().to_owned();
        parts.join(".")
    };
    assert!(matches!(v.verify(&forged).await, Err(JwtError::Invalid(_))));

    // Key b isn't published yet: unknown.
    let fetches = keys.1.load(Ordering::SeqCst);
    assert_eq!(
        v.verify(&sign("b", "aud1", &iss, 300)).await.unwrap_err(),
        JwtError::UnknownKey
    );
    // Repeated unknown kids don't refetch every time.
    let _ = v.verify(&sign("b", "aud1", &iss, 300)).await;
    assert!(keys.1.load(Ordering::SeqCst) <= fetches + 1);

    // Rotation: Cloudflare publishes b next to a; a token signed with b makes
    // the verifier look again (here without the refetch interval).
    let v = Verifier::new(TEAM, "aud1", keys.clone()).refetch_after(Duration::ZERO);
    assert!(v.verify(&sign("a", "aud1", &iss, 300)).await.is_ok());
    keys.0.lock().unwrap().push(jwk("b"));
    assert!(v.verify(&sign("b", "aud1", &iss, 300)).await.is_ok());
    // a is retired: tokens signed with it fail once the cache is refreshed.
    keys.0.lock().unwrap().remove(0);
    let v = Verifier::new(TEAM, "aud1", keys.clone());
    assert_eq!(
        v.verify(&sign("a", "aud1", &iss, 300)).await.unwrap_err(),
        JwtError::UnknownKey
    );
}

// ---------------------------------------------------------------- the gate

async fn gate_on(state: &AppState) {
    sqlx::query("UPDATE private_access SET phase = 'on', team_domain = $1, dashboard_aud = 'aud1'")
        .bind(TEAM)
        .execute(&state.db)
        .await
        .unwrap();
    let keys = Arc::new(Keys(Mutex::new(vec![jwk("a")]), AtomicU32::new(0)));
    state.private.set_hooks(|h| h.keys = keys);
    private::activate(state, &private::settings(&state.db).await.unwrap());
}

async fn get(router: axum::Router, path: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut b = Request::builder().uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = router
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = http_body_util::BodyExt::collect(res.into_body())
        .await
        .unwrap()
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn in_private_mode_requests_must_come_through_access(pool: PgPool) {
    let app = TestApp::new(pool);
    gate_on(&app.state).await;
    let iss = format!("https://{TEAM}");
    let remote = app.router.clone();

    // No token: refused, API and SPA alike.
    let (s, j) = get(remote.clone(), "/api/auth/me", &[]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(j["error"]["code"], "access_required");
    assert_eq!(get(remote.clone(), "/", &[]).await.0, StatusCode::FORBIDDEN);
    let bad = sign("a", "other-app", &iss, 300);
    assert_eq!(
        get(
            remote.clone(),
            "/api/auth/me",
            &[("cf-access-jwt-assertion", &bad)]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );

    // A valid token reaches the API (which then wants its own login).
    let good = sign("a", "aud1", &iss, 300);
    let (s, _) = get(
        remote.clone(),
        "/api/auth/me",
        &[("cf-access-jwt-assertion", &good)],
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(
        get(
            remote.clone(),
            "/api/health",
            &[("cf-access-jwt-assertion", &good)]
        )
        .await
        .0,
        StatusCode::OK
    );

    // Endpoints Access lets through are not refused.
    assert_ne!(
        get(remote.clone(), "/api/install.sh", &[]).await.0,
        StatusCode::FORBIDDEN
    );

    // Break glass: a direct local connection, without proxy headers.
    let local = modula_server::http::app(app.state.clone())
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 4000))));
    assert_eq!(
        get(local.clone(), "/api/health", &[]).await.0,
        StatusCode::OK
    );
    // cloudflared also connects from loopback, but always adds Cloudflare's headers.
    let (s, _) = get(
        local.clone(),
        "/api/health",
        &[("cf-ray", "abc"), ("cf-connecting-ip", "198.51.100.7")],
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = get(local, "/api/health", &[("x-forwarded-for", "198.51.100.7")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // Off: everything as before.
    sqlx::query("UPDATE private_access SET phase = 'off'")
        .execute(app.db())
        .await
        .unwrap();
    private::activate(&app.state, &private::settings(app.db()).await.unwrap());
    assert_eq!(get(remote, "/api/health", &[]).await.0, StatusCode::OK);
}

#[test]
fn access_redirects_are_recognised() {
    assert!(private::is_access_redirect(
        302,
        Some("https://acme.cloudflareaccess.com/cdn-cgi/access/login/x")
    ));
    assert!(!private::is_access_redirect(200, None));
    assert!(!private::is_access_redirect(
        302,
        Some("https://cloud.test/login")
    ));
}

// ---------------------------------------------------------------- CLI

/// Echoes the Access headers it got; `/challenge` answers like Access does.
async fn echo_server() -> url::Url {
    async fn echo(uri: Uri, headers: HeaderMap) -> axum::response::Response {
        if uri.path() == "/challenge" {
            return (
                StatusCode::FOUND,
                [(
                    "location",
                    "https://acme.cloudflareaccess.com/cdn-cgi/access/login/x",
                )],
            )
                .into_response();
        }
        let h = |k: &str| {
            headers
                .get(k)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        axum::Json(json!({
            "id": h("cf-access-client-id"),
            "secret": h("cf-access-client-secret"),
            "token": h("cf-access-token"),
            "auth": h("authorization"),
        }))
        .into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().fallback(echo))
            .await
            .unwrap()
    });
    format!("http://{addr}/").parse().unwrap()
}

#[tokio::test]
async fn the_cli_sends_access_credentials_and_reports_the_sign_in() {
    use modula_cli::access::CfAccess;
    use modula_cli::api::{Api, api_error};
    use modula_cli::config::Login;
    let url = echo_server().await;
    let api = |access| {
        Api::new(Login {
            url: url.clone(),
            token: "mdt_x".into(),
            access,
        })
        .unwrap()
    };

    let service = api(Some(CfAccess::ServiceToken {
        client_id: "cid.access".into(),
        client_secret: "sec".into(),
    }));
    let got: Value = service.get("/api/x").await.unwrap();
    assert_eq!(got["id"], "cid.access");
    assert_eq!(got["secret"], "sec");
    assert_eq!(got["auth"], "Bearer mdt_x");
    assert!(got["token"].is_null());

    let app = api(Some(CfAccess::AppToken {
        token: "jwt".into(),
    }));
    let got: Value = app.get("/api/x").await.unwrap();
    assert_eq!(got["token"], "jwt");
    assert!(got["id"].is_null());

    let none = api(None);
    let got: Value = none.get("/api/x").await.unwrap();
    assert!(got["token"].is_null() && got["id"].is_null());

    // Access's sign-in redirect is not followed: it becomes a clear error.
    let err = none.get::<Value>("/challenge").await.unwrap_err();
    let e = api_error(&err).expect("an API error");
    assert_eq!(e.code, "access_required");
    assert!(e.to_string().contains("MODULA_CF_ACCESS_CLIENT_ID"), "{e}");
}
