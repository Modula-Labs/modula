//! Static sites hosted on Cloudflare (`config.hosting`), end to end against
//! a mock of the Cloudflare API: a node builds the site (FakeBuilder), uploads
//! the output to the server over HTTP, and the publish job turns it into a
//! Worker with static assets, its variables and its custom domain.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use common::*;
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use modula_server::integrations::cloudflare::publish;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "cf-test-token-0123456789abcdefWXYZ";
const SESSION_JWT: &str = "session-jwt";
const DONE_JWT: &str = "completion-jwt";

#[derive(Default)]
struct Cf {
    calls: Mutex<Vec<(Method, String)>>,
    records: Mutex<Vec<Value>>,
    next_id: AtomicU32,
    /// The token lacks Workers Scripts · Edit.
    no_workers: AtomicBool,
    manifest: Mutex<Value>,
    uploaded: Mutex<Vec<String>>,
    script: Mutex<String>,
    domains: Mutex<Vec<Value>>,
}

impl Cf {
    fn called(&self, method: &str, suffix: &str) -> bool {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .any(|(m, u)| m.as_str() == method && u.split('?').next().unwrap().ends_with(suffix))
    }
}

fn ok(result: Value) -> axum::response::Response {
    axum::Json(json!({ "success": true, "errors": [], "messages": [], "result": result }))
        .into_response()
}

fn fail(status: StatusCode, code: i64, message: &str) -> axum::response::Response {
    (
        status,
        axum::Json(json!({ "success": false, "errors": [{ "code": code, "message": message }], "result": null })),
    )
        .into_response()
}

/// The `name="…"` parts of a multipart body, with their contents.
fn parts(headers: &HeaderMap, body: &[u8]) -> Vec<(String, String)> {
    let ct = headers["content-type"].to_str().unwrap();
    let boundary = ct.split("boundary=").nth(1).unwrap();
    let text = String::from_utf8_lossy(body);
    text.split(&format!("--{boundary}"))
        .filter_map(|p| {
            let (head, data) = p.split_once("\r\n\r\n")?;
            let name = head.split("name=\"").nth(1)?.split('"').next()?.to_owned();
            Some((name, data.trim_end_matches("\r\n").to_owned()))
        })
        .collect()
}

async fn handle(
    State(cf): State<Arc<Cf>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let path = uri.path().trim_start_matches("/client/v4/").to_owned();
    let query = uri.query().unwrap_or_default().to_owned();
    cf.calls
        .lock()
        .unwrap()
        .push((method.clone(), uri.to_string()));
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let seg: Vec<&str> = path.split('/').collect();
    // Asset uploads are authorized by the session JWT, everything else by the token.
    if matches!(seg[..], ["accounts", _, "workers", "assets", "upload"]) {
        if auth != format!("Bearer {SESSION_JWT}") || query != "base64=true" {
            return fail(StatusCode::UNAUTHORIZED, 10001, "bad upload auth");
        }
        let got = parts(&headers, &body);
        let mut up = cf.uploaded.lock().unwrap();
        for (hash, data) in got {
            use base64::Engine as _;
            assert!(
                base64::engine::general_purpose::STANDARD
                    .decode(&data)
                    .is_ok(),
                "contents are base64"
            );
            up.push(hash);
        }
        return (
            StatusCode::CREATED,
            axum::Json(json!({ "success": true, "errors": [], "result": { "jwt": DONE_JWT } })),
        )
            .into_response();
    }
    if auth != format!("Bearer {TOKEN}") {
        return fail(StatusCode::UNAUTHORIZED, 1000, "Invalid API Token");
    }
    let json_body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    match (method.as_str(), &seg[..]) {
        ("GET", ["user", "tokens", "verify"]) => ok(json!({ "id": "tok1", "status": "active" })),
        ("GET", ["accounts"]) => ok(json!([{ "id": "acc1", "name": "Acme" }])),
        ("GET", ["zones"]) => {
            let all = json!([{ "id": "zid1", "name": "example.com", "status": "active",
                               "account": { "id": "acc1", "name": "Acme" } }]);
            let name = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("name="))
                .map(str::to_owned);
            match name {
                Some(n) if n != "example.com" => ok(json!([])),
                _ => ok(all),
            }
        }
        ("GET", ["zones", _, "settings", "ssl"]) => ok(json!({ "value": "strict" })),
        ("GET", ["zones", _, "dns_records"]) => {
            let recs: Vec<Value> = cf
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| query.contains(&format!("type={}", r["type"].as_str().unwrap())))
                .cloned()
                .collect();
            ok(json!(recs))
        }
        ("POST", ["zones", _, "dns_records"]) => {
            let mut r = json_body.clone();
            r["id"] = json!(format!("rec{}", cf.next_id.fetch_add(1, Ordering::SeqCst)));
            cf.records.lock().unwrap().push(r.clone());
            ok(r)
        }
        ("DELETE", ["zones", _, "dns_records", id]) => {
            cf.records.lock().unwrap().retain(|r| r["id"] != *id);
            ok(json!({ "id": id }))
        }
        (_, ["accounts", "acc1", "workers", ..]) if cf.no_workers.load(Ordering::SeqCst) => {
            fail(StatusCode::FORBIDDEN, 10000, "Authentication error")
        }
        (
            "POST",
            [
                "accounts",
                "acc1",
                "workers",
                "scripts",
                _,
                "assets-upload-session",
            ],
        ) => {
            *cf.manifest.lock().unwrap() = json_body["manifest"].clone();
            let hashes: Vec<Value> = json_body["manifest"]
                .as_object()
                .unwrap()
                .values()
                .map(|v| v["hash"].clone())
                .collect();
            ok(json!({ "jwt": SESSION_JWT, "buckets": [hashes] }))
        }
        ("PUT", ["accounts", "acc1", "workers", "scripts", name]) => {
            let p = parts(&headers, &body);
            let meta = &p.iter().find(|(n, _)| n == "metadata").unwrap().1;
            let meta: Value = serde_json::from_str(meta).unwrap();
            assert_eq!(meta["assets"]["jwt"], DONE_JWT);
            assert!(
                p.iter()
                    .any(|(n, d)| n == "worker.js" && d.contains("env.ASSETS.fetch"))
            );
            *cf.script.lock().unwrap() = format!("{name} {meta}");
            ok(json!({ "id": name }))
        }
        ("GET", ["accounts", "acc1", "workers", "subdomain"]) => ok(json!({ "subdomain": "acme" })),
        ("POST", ["accounts", "acc1", "workers", "scripts", _, "subdomain"]) => {
            ok(json!({ "enabled": true }))
        }
        ("PUT", ["accounts", "acc1", "workers", "domains"]) => {
            cf.domains.lock().unwrap().push(json_body.clone());
            ok(
                json!({ "id": "dom1", "hostname": json_body["hostname"], "service": json_body["service"] }),
            )
        }
        _ => fail(StatusCode::NOT_FOUND, 7003, "Could not route"),
    }
}

async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn mock_github() -> url::Url {
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/repos/acme/site/commits/{branch}",
        get(|| async {
            axum::Json(json!({
                "sha": "0123456789abcdef0123456789abcdef01234567",
                "commit": { "message": "Landing page", "author": { "name": "Ada" } }
            }))
        }),
    );
    format!("http://{}", serve(app).await).parse().unwrap()
}

async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..300 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

struct Setup {
    app: TestApp,
    admin: String,
    cf: Arc<Cf>,
    rt: Arc<FakeRuntime>,
    stop: CancellationToken,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// A server with its HTTP API on a real port (nodes upload to it), a job
/// worker running the publish job, and one node with a fake builder.
async fn setup(pool: PgPool) -> Setup {
    let cf = Arc::new(Cf::default());
    let cf_addr = serve(axum::Router::new().fallback(handle).with_state(cf.clone())).await;
    let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api.local_addr().unwrap();
    let mut cfg = config();
    cfg.github_api = mock_github().await;
    cfg.cloudflare_api = format!("http://{cf_addr}/client/v4").parse().unwrap();
    cfg.internal_url = Some(format!("http://{api_addr}").parse().unwrap());
    let app = TestApp::with_config(pool, cfg);
    let router = app.router.clone();
    tokio::spawn(async move { axum::serve(api, router).await.unwrap() });
    let admin = app.setup_admin().await;
    let stop = CancellationToken::new();
    tokio::spawn(modula_server::jobs::run_worker(
        app.state.clone(),
        modula_server::jobs::Registry::default().on(publish::JOB, publish::job),
        "test-worker".into(),
        stop.clone(),
    ));

    // One node.
    let nodes = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = nodes.local_addr().unwrap().port();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        nodes,
        stop.clone(),
    ));
    let t = app
        .call(Method::POST, "/api/nodes/join-tokens", Some(&admin), None)
        .await
        .json;
    let dir = std::env::temp_dir().join(format!("modula-cf-hosting-{}", Id::new()));
    join(JoinArgs {
        server: &format!("https://localhost:{port}"),
        token: t["token"].as_str().unwrap(),
        fingerprint: t["fingerprint"].as_str().unwrap(),
        hostname: "builder".into(),
        dir: &dir,
    })
    .await
    .expect("join");
    let rt = Arc::new(FakeRuntime::new());
    let caps = modula_node::reconcile::Capabilities {
        build: Some(modula_node::build::BuildSupport {
            builder: Arc::new(modula_node::build::FakeBuilder::default()),
            logs: modula_node::build::BuildLogs::new(dir.join("build-logs")).unwrap(),
        }),
        ..Default::default()
    };
    tokio::spawn(modula_node::session::run(
        dir,
        rt.clone(),
        caps,
        stop.clone(),
    ));
    let s = Setup {
        app,
        admin,
        cf,
        rt,
        stop,
    };
    wait_for("node online", || async {
        let r = s
            .app
            .call(Method::GET, "/api/nodes", Some(&s.admin), None)
            .await;
        r.json
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["connected"] == true)
    })
    .await;
    s
}

impl Setup {
    async fn workspace(&self, connect: bool) -> Id {
        let ws = self.app.create_workspace(&self.admin, "team").await;
        if connect {
            let r = self
                .app
                .call(
                    Method::PUT,
                    &format!("/api/workspaces/{ws}/integrations/cloudflare"),
                    Some(&self.admin),
                    Some(json!({ "token": TOKEN })),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        }
        ws
    }

    async fn site(&self, ws: Id) -> Id {
        let p = self
            .app
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(&self.admin),
                Some(json!({"name": "marketing"})),
            )
            .await;
        let project = p.json["id"].as_str().unwrap().to_owned();
        let s = self
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{project}/services"),
                Some(&self.admin),
                Some(json!({ "name": "web", "config": {
                    "source": { "type": "repo", "repo": "acme/site", "branch": "main", "rootDir": "apps/web" },
                    "buildCommand": "npm run build",
                    "hosting": { "type": "cloudflare", "outputDir": "dist", "spa": true }
                }})),
            )
            .await;
        assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
        s.json["instances"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap()
    }

    async fn deploy(&self, instance: Id) -> axum::http::StatusCode {
        self.app
            .call(
                Method::POST,
                &format!("/api/service-instances/{instance}/deployments"),
                Some(&self.admin),
                None,
            )
            .await
            .status
    }

    async fn latest(&self, instance: Id) -> Value {
        let r = self
            .app
            .call(
                Method::GET,
                &format!("/api/service-instances/{instance}/deployments"),
                Some(&self.admin),
                None,
            )
            .await;
        let id = r.json.as_array().unwrap()[0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        self.app
            .call(
                Method::GET,
                &format!("/api/deployments/{id}"),
                Some(&self.admin),
                None,
            )
            .await
            .json
    }

    async fn wait_status(&self, instance: Id, want: &str) -> Value {
        let mut last = Value::Null;
        for _ in 0..300 {
            last = self.latest(instance).await;
            if last["status"] == want {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("deployment never became {want}: {last}");
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_static_site_is_built_on_a_node_and_published_as_a_worker(pool: PgPool) {
    let s = setup(pool).await;
    let ws = s.workspace(true).await;
    let instance = s.site(ws).await;
    let r = s
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}/variables"),
            Some(&s.admin),
            Some(json!({
                "set": { "API_URL": "https://api.example.com", "SIGNING_KEY": "k", "VITE_API_URL": "https://api.example.com" },
                "sealed": ["SIGNING_KEY"]
            })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/custom-domains"),
            Some(&s.admin),
            Some(json!({ "hostname": "www.example.com" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    assert_eq!(s.deploy(instance).await, StatusCode::OK);
    let d = s.wait_status(instance, "active").await;

    // Served from Cloudflare, at the custom domain.
    assert_eq!(d["url"], "https://www.example.com", "{d}");
    let phases: Vec<&str> = d["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["phase"].as_str().unwrap())
        .collect();
    assert!(
        phases.starts_with(&["queued", "building", "built", "publishing"]),
        "{phases:?}"
    );
    assert_eq!(phases.last(), Some(&"active"));
    assert!(
        s.rt.running_deployments().is_empty(),
        "nothing runs on the node"
    );

    // The manifest has the output's files; every one was uploaded.
    let manifest = s.cf.manifest.lock().unwrap().clone();
    let paths: Vec<&String> = manifest.as_object().unwrap().keys().collect();
    assert_eq!(paths, ["/assets/app.js", "/index.html"]);
    assert_eq!(s.cf.uploaded.lock().unwrap().len(), 2);

    // The Worker: SPA fallback, variables as plain text and secrets, build-only ones left out.
    let script = s.cf.script.lock().unwrap().clone();
    let (name, meta) = script.split_once(' ').unwrap();
    assert_eq!(name, "web-production");
    let meta: Value = serde_json::from_str(meta).unwrap();
    assert_eq!(
        meta["assets"]["config"]["not_found_handling"],
        "single-page-application"
    );
    let bindings = meta["bindings"].as_array().unwrap();
    let kind = |n: &str| {
        bindings
            .iter()
            .find(|b| b["name"] == n)
            .map(|b| b["type"].as_str().unwrap().to_owned())
    };
    assert_eq!(kind("ASSETS").as_deref(), Some("assets"));
    assert_eq!(kind("API_URL").as_deref(), Some("plain_text"));
    assert_eq!(kind("SIGNING_KEY").as_deref(), Some("secret_text"));
    assert_eq!(kind("VITE_API_URL"), None);
    assert_eq!(kind("PORT"), None, "platform variables stay out");

    // workers.dev and the custom domain; Modula's own routing record is gone.
    assert!(s.cf.called("POST", "workers/scripts/web-production/subdomain"));
    let domains = s.cf.domains.lock().unwrap().clone();
    assert_eq!(domains.len(), 1);
    assert_eq!(
        (
            domains[0]["hostname"].as_str(),
            domains[0]["service"].as_str(),
            domains[0]["zone_id"].as_str()
        ),
        (
            Some("www.example.com"),
            Some("web-production"),
            Some("zid1")
        )
    );
    assert!(
        !s.cf
            .records
            .lock()
            .unwrap()
            .iter()
            .any(|r| r["type"] == "CNAME" && r["name"] == "www.example.com"),
        "the CNAME to this instance's edge was removed"
    );
    let r = s
        .app
        .call(
            Method::GET,
            &format!("/api/service-instances/{instance}/domains"),
            Some(&s.admin),
            None,
        )
        .await;
    let custom = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["hostname"] == "www.example.com")
        .unwrap()
        .clone();
    assert_eq!(custom["status"], "verified", "{custom}");

    // The output archive is gone once published.
    let outputs = s.app.state.cfg.uploads_dir.join("outputs");
    let left: Vec<_> = std::fs::read_dir(&outputs)
        .map(|d| d.flatten().collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");

    // A redeploy publishes again and retires the previous one.
    let first = d["id"].as_str().unwrap().to_owned();
    assert_eq!(s.deploy(instance).await, StatusCode::OK);
    let d2 = s.wait_status(instance, "active").await;
    assert_ne!(d2["id"].as_str().unwrap(), first);
    let old = s
        .app
        .call(
            Method::GET,
            &format!("/api/deployments/{first}"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(old.json["status"], "removed");

    // Restarting makes no sense for a site on Cloudflare.
    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/deployments/{}/restart", d2["id"].as_str().unwrap()),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn missing_connection_or_permission_is_a_clear_error(pool: PgPool) {
    let s = setup(pool).await;

    // Without Cloudflare, the deploy is refused before anything builds.
    let ws = s.workspace(false).await;
    let instance = s.site(ws).await;
    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/deployments"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(
        r.json.to_string().contains("Cloudflare isn't connected"),
        "{:?}",
        r.json
    );

    // A token without Workers Scripts · Edit fails the deployment, saying so.
    let ws = s.workspace(true).await;
    let instance = s.site(ws).await;
    s.cf.no_workers.store(true, Ordering::SeqCst);
    assert_eq!(s.deploy(instance).await, StatusCode::OK);
    let d = s.wait_status(instance, "failed").await;
    assert!(
        d["error"]
            .as_str()
            .unwrap()
            .contains("Account · Workers Scripts · Edit"),
        "{d}"
    );

    // Image sources can't be hosted on Cloudflare.
    let p = s
        .app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(&s.admin),
            Some(json!({"name": "other"})),
        )
        .await;
    let project = p.json["id"].as_str().unwrap().to_owned();
    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/projects/{project}/services"),
            Some(&s.admin),
            Some(json!({ "name": "img", "config": {
                "source": { "type": "image", "image": "nginx:1" },
                "hosting": { "type": "cloudflare", "outputDir": "dist" }
            }})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn build_output_uploads_need_a_valid_token_for_a_building_deployment(pool: PgPool) {
    let app = TestApp::new(pool);
    let put = |auth: Option<String>| {
        let mut b = axum::http::Request::builder()
            .method(Method::PUT)
            .uri("/api/build-output")
            .header("content-type", "application/gzip");
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        b
    };
    let r = app.send(put(None), None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = app.send(put(Some("Bearer nope".into())), None, None).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // A genuine token for a deployment that doesn't exist (or isn't building).
    let t = modula_server::deploy::output::token(
        &app.state.keys,
        Id::new(),
        time::OffsetDateTime::now_utc(),
    );
    let r = app.send(put(Some(format!("Bearer {t}"))), None, None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}
