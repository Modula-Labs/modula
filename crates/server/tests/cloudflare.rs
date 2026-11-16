//! Cloudflare integration against an in-process mock of the v4 API: the
//! client (envelopes, pagination, 429, idempotent records) and the API
//! (connect/sudo/roles, the token never coming back, DNS automation for
//! custom domains, proxied mode).

mod common;

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use common::*;
use modula_id::Id;
use modula_server::integrations::cloudflare::client::{Change, Client};
use secrecy::SecretString;
use serde_json::{Value, json};
use sqlx::PgPool;

const TOKEN: &str = "cf-test-token-0123456789abcdefWXYZ";

#[derive(Default)]
struct Cf {
    calls: Mutex<Vec<(Method, String)>>,
    records: Mutex<Vec<Value>>,
    next_id: AtomicU32,
    r2_allowed: AtomicBool,
    buckets: Mutex<Vec<String>>,
    /// Answer the next request with 429 (Retry-After: 0).
    throttle_next: AtomicBool,
    /// Writes answer success but change nothing (read-back must catch it).
    ignore_writes: AtomicBool,
}

impl Cf {
    fn writes(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| *m != Method::GET)
            .count()
    }

    fn records(&self) -> Vec<Value> {
        self.records.lock().unwrap().clone()
    }
}

fn ok(result: Value) -> axum::response::Response {
    axum::Json(json!({ "success": true, "errors": [], "messages": [], "result": result }))
        .into_response()
}

fn page(items: Vec<Value>, page: u32, total_pages: u32) -> axum::response::Response {
    axum::Json(json!({
        "success": true, "errors": [], "result": items,
        "result_info": { "page": page, "per_page": 50, "total_pages": total_pages, "count": items.len() }
    }))
    .into_response()
}

fn fail(status: StatusCode, code: i64, message: &str) -> axum::response::Response {
    (
        status,
        axum::Json(json!({ "success": false, "errors": [{ "code": code, "message": message }], "result": null })),
    )
        .into_response()
}

fn zones() -> Vec<Value> {
    vec![
        json!({ "id": "zid1", "name": "example.com", "status": "active", "paused": false,
                "account": { "id": "acc1", "name": "Acme" } }),
        json!({ "id": "zid2", "name": "shop.dev", "status": "active", "paused": false,
                "account": { "id": "acc1", "name": "Acme" } }),
    ]
}

async fn handle(
    State(cf): State<Arc<Cf>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let path = uri.path().trim_start_matches("/client/v4/").to_owned();
    let query: std::collections::HashMap<String, String> = uri
        .query()
        .map(|q| {
            url::form_urlencoded::parse(q.as_bytes())
                .into_owned()
                .collect()
        })
        .unwrap_or_default();
    cf.calls
        .lock()
        .unwrap()
        .push((method.clone(), uri.to_string()));
    if cf.throttle_next.swap(false, Ordering::SeqCst) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "0")],
            "slow down",
        )
            .into_response();
    }
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {TOKEN}"))
    {
        return fail(StatusCode::UNAUTHORIZED, 1000, "Invalid API Token");
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let seg: Vec<&str> = path.split('/').collect();
    match (method.as_str(), &seg[..]) {
        ("GET", ["user", "tokens", "verify"]) => ok(json!({ "id": "tok1", "status": "active" })),
        ("GET", ["accounts"]) => page(vec![json!({ "id": "acc1", "name": "Acme" })], 1, 1),
        ("GET", ["zones"]) => match query.get("name") {
            Some(n) => page(
                zones().into_iter().filter(|z| z["name"] == **n).collect(),
                1,
                1,
            ),
            // One zone per page, to exercise pagination.
            None => {
                let p: usize = query.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                page(
                    zones().into_iter().skip(p - 1).take(1).collect(),
                    p as u32,
                    2,
                )
            }
        },
        ("GET", ["zones", "zid1", "settings", "ssl"]) => {
            ok(json!({ "id": "ssl", "value": "flexible" }))
        }
        ("GET", ["zones", _, "settings", "ssl"]) => {
            fail(StatusCode::FORBIDDEN, 9109, "Unauthorized")
        }
        ("GET", ["zones", zone, "dns_records"]) => {
            let found = cf
                .records()
                .into_iter()
                .filter(|r| r["zone"] == *zone)
                .filter(|r| query.get("type").is_none_or(|t| r["type"] == **t))
                .filter(|r| query.get("name").is_none_or(|n| r["name"] == **n))
                .collect();
            let found: Vec<Value> = found;
            let p: usize = query.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
            let total = found.len().div_ceil(50).max(1);
            page(
                found.into_iter().skip((p - 1) * 50).take(50).collect(),
                p as u32,
                total as u32,
            )
        }
        ("POST", ["zones", zone, "dns_records"]) => {
            let id = format!("rec{}", cf.next_id.fetch_add(1, Ordering::SeqCst));
            let mut r = body.clone();
            r["id"] = json!(id);
            r["zone"] = json!(zone);
            // Cloudflare returns TXT content quoted.
            if r["type"] == "TXT" {
                r["content"] = json!(format!("\"{}\"", r["content"].as_str().unwrap()));
            }
            cf.records.lock().unwrap().push(r.clone());
            ok(r)
        }
        ("PATCH", ["zones", _, "dns_records", id]) => {
            let mut recs = cf.records.lock().unwrap();
            if cf.ignore_writes.load(Ordering::SeqCst) {
                return ok(json!({ "id": id }));
            }
            let me = recs.iter().find(|r| r["id"] == *id).cloned();
            if let Some(me) = &me
                && recs.iter().any(|r| {
                    r["id"] != *id
                        && r["name"] == me["name"]
                        && r["type"] == me["type"]
                        && r["content"] == body["content"]
                })
            {
                return fail(
                    StatusCode::BAD_REQUEST,
                    81058,
                    "An identical record already exists.",
                );
            }
            let Some(r) = recs.iter_mut().find(|r| r["id"] == *id) else {
                return fail(StatusCode::NOT_FOUND, 81044, "Record does not exist.");
            };
            for (k, v) in body.as_object().unwrap() {
                r[k] = v.clone();
            }
            ok(r.clone())
        }
        ("DELETE", ["zones", _, "dns_records", id]) => {
            let mut recs = cf.records.lock().unwrap();
            let before = recs.len();
            recs.retain(|r| r["id"] != *id);
            if recs.len() == before {
                return fail(StatusCode::NOT_FOUND, 81044, "Record does not exist.");
            }
            ok(json!({ "id": id }))
        }
        (_, ["accounts", "acc1", "r2", "buckets"]) if !cf.r2_allowed.load(Ordering::SeqCst) => {
            fail(StatusCode::FORBIDDEN, 10000, "Authentication error")
        }
        ("GET", ["accounts", "acc1", "r2", "buckets"]) => ok(json!({ "buckets": [] })),
        ("POST", ["accounts", "acc1", "r2", "buckets"]) => {
            let name = body["name"].as_str().unwrap().to_owned();
            let mut b = cf.buckets.lock().unwrap();
            if b.contains(&name) {
                return fail(
                    StatusCode::CONFLICT,
                    10004,
                    "The bucket you tried to create already exists",
                );
            }
            b.push(name.clone());
            ok(json!({ "name": name }))
        }
        _ => fail(StatusCode::NOT_FOUND, 7003, "Could not route"),
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

fn client(base: &url::Url, token: &str) -> Client {
    Client::new(base.clone(), SecretString::from(token.to_owned()))
}

// ---------------------------------------------------------------- client

#[tokio::test]
async fn client_verifies_lists_and_reports_errors() {
    let (base, cf) = mock_cloudflare().await;
    let c = client(&base, TOKEN);
    assert_eq!(c.verify().await.unwrap().status, "active");

    // Both pages of zones.
    let names: Vec<String> = c
        .zones()
        .await
        .unwrap()
        .into_iter()
        .map(|z| z.name)
        .collect();
    assert_eq!(names, ["example.com", "shop.dev"]);
    assert_eq!(c.accounts().await.unwrap()[0].id, "acc1");

    // The deepest matching parent zone; unknown names have none.
    let z = c.find_zone("a.b.example.com").await.unwrap().unwrap();
    assert_eq!(z.id, "zid1");
    assert_eq!(c.find_zone("*.shop.dev").await.unwrap().unwrap().id, "zid2");
    assert!(c.find_zone("nope.org").await.unwrap().is_none());
    assert_eq!(c.ssl_mode("zid1").await.unwrap(), "flexible");

    // Error envelopes become typed errors with Cloudflare's message.
    let e = client(&base, "wrong-token-wrong-token")
        .verify()
        .await
        .unwrap_err();
    assert_eq!(e.status(), Some(401));
    assert!(e.is_forbidden());
    assert!(e.to_string().contains("Invalid API Token"), "{e}");

    // A 429 is retried once.
    cf.throttle_next.store(true, Ordering::SeqCst);
    let before = cf.calls.lock().unwrap().len();
    assert_eq!(c.verify().await.unwrap().id, "tok1");
    assert_eq!(cf.calls.lock().unwrap().len(), before + 2);
}

#[tokio::test]
async fn ensure_record_is_idempotent_and_delete_cleans_up() {
    let (base, cf) = mock_cloudflare().await;
    let c = client(&base, TOKEN);

    let cname = || {
        c.ensure_record(
            "zid1",
            "www.example.com",
            "CNAME",
            "web-1a2b.apps.test",
            false,
        )
    };
    assert_eq!(cname().await.unwrap(), Change::Created);
    let writes = cf.writes();
    assert_eq!(cname().await.unwrap(), Change::Unchanged);
    assert_eq!(cf.writes(), writes, "an identical record makes no write");

    // TXT comes back quoted and still compares equal.
    let txt = || {
        c.ensure_record(
            "zid1",
            "_modula-challenge.www.example.com",
            "TXT",
            "modula-verify=ab",
            true,
        )
    };
    assert_eq!(txt().await.unwrap(), Change::Created);
    assert_eq!(txt().await.unwrap(), Change::Unchanged);
    let rec = cf
        .records()
        .into_iter()
        .find(|r| r["type"] == "TXT")
        .unwrap();
    assert!(
        rec.get("proxied").is_none(),
        "TXT records are never proxied"
    );

    // Proxying, and pointing the CNAME elsewhere, update in place.
    assert_eq!(
        c.ensure_record(
            "zid1",
            "www.example.com",
            "CNAME",
            "web-1a2b.apps.test",
            true
        )
        .await
        .unwrap(),
        Change::Updated
    );
    assert_eq!(
        c.ensure_record("zid1", "www.example.com", "cname", "other.apps.test", true)
            .await
            .unwrap(),
        Change::Updated
    );
    let cnames: Vec<Value> = cf
        .records()
        .into_iter()
        .filter(|r| r["type"] == "CNAME")
        .collect();
    assert_eq!(cnames.len(), 1);
    assert_eq!(
        (
            cnames[0]["content"].as_str(),
            cnames[0]["proxied"].as_bool()
        ),
        (Some("other.apps.test"), Some(true))
    );

    // A records accumulate (several servers per name).
    c.ensure_record("zid1", "example.com", "A", "203.0.113.7", false)
        .await
        .unwrap();
    c.ensure_record("zid1", "example.com", "A", "203.0.113.8", false)
        .await
        .unwrap();
    assert_eq!(cf.records().iter().filter(|r| r["type"] == "A").count(), 2);

    // The user's own record (no Modula comment) is never deleted.
    cf.records.lock().unwrap().push(json!({
        "id": "theirs", "zone": "zid1", "type": "A", "name": "example.com", "content": "198.51.100.1"
    }));
    assert_eq!(
        c.delete_records("zid1", "example.com", "A").await.unwrap(),
        2
    );
    assert_eq!(
        c.delete_records("zid1", "example.com", "A").await.unwrap(),
        0
    );
    assert_eq!(
        c.delete_records("zid1", "www.example.com", "CNAME")
            .await
            .unwrap(),
        1
    );
    let left: Vec<Value> = cf.records();
    assert_eq!(left.len(), 2, "{left:?}");
    assert!(left.iter().any(|r| r["id"] == "theirs"));
}

#[tokio::test]
async fn r2_buckets() {
    let (base, cf) = mock_cloudflare().await;
    let c = client(&base, TOKEN);
    assert!(!c.r2_access("acc1").await.unwrap());
    assert!(
        c.create_r2_bucket("acc1", "backups")
            .await
            .unwrap_err()
            .is_forbidden()
    );
    cf.r2_allowed.store(true, Ordering::SeqCst);
    assert!(c.r2_access("acc1").await.unwrap());
    assert_eq!(
        c.create_r2_bucket("acc1", "backups").await.unwrap(),
        Change::Created
    );
    assert_eq!(
        c.create_r2_bucket("acc1", "backups").await.unwrap(),
        Change::Unchanged
    );
}

// ---------------------------------------------------------------- API

async fn app_with_cf(pool: PgPool) -> (TestApp, Arc<Cf>) {
    let (base, cf) = mock_cloudflare().await;
    let mut cfg = config();
    cfg.cloudflare_api = base;
    (TestApp::with_config(pool, cfg), cf)
}

fn cf_path(ws: Id) -> String {
    format!("/api/workspaces/{ws}/integrations/cloudflare")
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn connecting_seals_the_token_and_never_returns_it(pool: PgPool) {
    let (app, _cf) = app_with_cf(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let path = cf_path(ws);

    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["connected"], false);
    assert!(
        r.json["tokenTemplateUrl"]
            .as_str()
            .unwrap()
            .starts_with("https://dash.cloudflare.com/")
    );

    // A rejected token is a plain 400 and stores nothing.
    let r = app
        .call(
            Method::PUT,
            &path,
            Some(&admin),
            Some(json!({ "token": "x".repeat(40) })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    // Verify previews without storing.
    let r = app
        .call(
            Method::POST,
            &format!("{path}/verify"),
            Some(&admin),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["accountId"], "acc1");
    assert_eq!(r.json["zones"].as_array().unwrap().len(), 2);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM integrations")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(n, 0);

    let r = app
        .call(
            Method::PUT,
            &path,
            Some(&admin),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["connected"], true);
    assert_eq!(r.json["accountName"], "Acme");
    assert_eq!(r.json["tokenHint"], "…WXYZ");
    assert!(!r.json.to_string().contains(TOKEN));

    let (sealed, metadata): (Vec<u8>, Value) =
        sqlx::query_as("SELECT secret_sealed, metadata FROM integrations WHERE workspace_id = $1 AND kind = 'cloudflare'")
            .bind(ws)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(!sealed.is_empty());
    assert!(!sealed.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()));
    assert!(!metadata.to_string().contains(TOKEN));
    let conn = modula_server::integrations::cloudflare::connection(&app.state, ws)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(conn.metadata.account_id.as_deref(), Some("acc1"));

    // Nothing readable returns the token, and the audit log doesn't hold it.
    for p in [path.clone(), format!("{path}/zones")] {
        let r = app.call(Method::GET, &p, Some(&admin), None).await;
        assert_eq!(r.status, StatusCode::OK, "{p}: {:?}", r.json);
        assert!(!r.json.to_string().contains(TOKEN));
    }
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, metadata FROM audit_log WHERE action LIKE 'integration.%' ORDER BY id",
    )
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].0, "integration.connect");
    assert!(!audit[0].1.to_string().contains(TOKEN));

    // Replacing keeps the row (its id is bound into the AAD) and still opens.
    let r = app
        .call(
            Method::PUT,
            &path,
            Some(&admin),
            Some(json!({ "token": TOKEN, "accountId": "acc1" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let again = modula_server::integrations::cloudflare::connection(&app.state, ws)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.id, conn.id);

    let r = app.call(Method::DELETE, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["connected"], false);
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE action LIKE 'integration.%' ORDER BY id",
    )
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(
        actions,
        [
            "integration.connect",
            "integration.update",
            "integration.disconnect"
        ]
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn only_admins_in_sudo_can_connect(pool: PgPool) {
    let (app, _cf) = app_with_cf(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let path = cf_path(ws);
    let (member_id, member) = app.user("member@example.com").await;
    app.add_member(ws, member_id, "member").await;
    let (viewer_id, viewer) = app.user("viewer@example.com").await;
    app.add_member(ws, viewer_id, "viewer").await;

    let body = || Some(json!({ "token": TOKEN }));
    for (method, p) in [
        (Method::PUT, path.clone()),
        (Method::POST, format!("{path}/verify")),
        (Method::DELETE, path.clone()),
    ] {
        let r = app.call(method.clone(), &p, Some(&member), body()).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{method} {p}");
    }
    // Members may see the status (no token), viewers may not.
    assert_eq!(
        app.call(Method::GET, &path, Some(&member), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.call(Method::GET, &path, Some(&viewer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    app.drop_sudo().await;
    let r = app.call(Method::PUT, &path, Some(&admin), body()).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "sudo_required");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM integrations")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn custom_domains_get_dns_records_and_a_proxied_toggle(pool: PgPool) {
    let (app, cf) = app_with_cf(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let chain = app.seed_chain(ws).await;
    let r = app
        .call(
            Method::PUT,
            &cf_path(ws),
            Some(&admin),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    // Creating a custom domain in a Cloudflare zone writes its records.
    let r = app
        .call(
            Method::POST,
            &format!("/api/service-instances/{}/custom-domains", chain.instance),
            Some(&admin),
            Some(json!({ "hostname": "www.example.com" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(
        r.json.get("providerNote").is_none_or(Value::is_null),
        "{:?}",
        r.json
    );
    let domain: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    let recs = cf.records();
    let cname = recs
        .iter()
        .find(|r| r["type"] == "CNAME")
        .expect("CNAME written");
    assert_eq!(cname["name"], "www.example.com");
    assert_eq!(cname["proxied"], false);
    assert!(
        recs.iter()
            .any(|r| r["type"] == "TXT" && r["name"] == "_modula-challenge.www.example.com")
    );

    let ingress = format!("/api/domains/{domain}/ingress");
    let r = app.call(Method::GET, &ingress, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        (r.json["managed"].as_bool(), r.json["zone"].as_str()),
        (Some(true), Some("example.com"))
    );
    assert_eq!(r.json["proxied"], false);
    assert!(r.json["sslWarning"].is_null());

    // Proxying flips the routing record (not the TXT) and warns about Flexible SSL.
    let r = app
        .call(
            Method::PUT,
            &ingress,
            Some(&admin),
            Some(json!({ "proxied": true })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["proxied"], true);
    assert_eq!(r.json["sslMode"], "flexible");
    assert!(
        r.json["sslWarning"]
            .as_str()
            .unwrap()
            .contains("Full (strict)")
    );
    let recs = cf.records();
    assert_eq!(
        recs.iter().find(|r| r["type"] == "CNAME").unwrap()["proxied"],
        true
    );
    assert!(
        recs.iter()
            .find(|r| r["type"] == "TXT")
            .unwrap()
            .get("proxied")
            .is_none()
    );
    let proxied: bool = sqlx::query_scalar("SELECT proxied FROM domains WHERE id = $1")
        .bind(domain)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert!(proxied);

    // Deleting the domain removes its records.
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/domains/{domain}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(cf.records().is_empty(), "{:?}", cf.records());

    // Without R2 permission the shortcut fails before touching backups.
    let r = app
        .call(
            Method::POST,
            &format!("{}/r2-targets", cf_path(ws)),
            Some(&admin),
            Some(json!({ "name": "r2", "bucket": "modula-backups", "accessKeyId": "k", "secretAccessKey": "s" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    assert!(r.json["error"]["message"].as_str().unwrap().contains("R2"));
    let r = app
        .call(
            Method::GET,
            &format!("{}/r2", cf_path(ws)),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["endpoint"], "https://acc1.r2.cloudflarestorage.com");
    assert_eq!(r.json["canCreateBuckets"], false);
}

// ---------------------------------------------------------------- following a server

const OLD: &str = "198.51.100.1";
const NEW: &str = "198.51.100.2";

fn seed(cf: &Cf, zone: &str, id: &str, name: &str, content: &str, comment: Option<&str>) {
    cf.records.lock().unwrap().push(json!({
        "id": id, "zone": zone, "type": "A", "name": name, "content": content,
        "proxied": true, "ttl": 300, "comment": comment,
    }));
}

fn content_of(cf: &Cf, id: &str) -> Option<Value> {
    cf.records().into_iter().find(|r| r["id"] == id)
}

async fn connect(app: &TestApp, admin: &str, ws: Id) {
    let r = app
        .call(
            Method::PUT,
            &cf_path(ws),
            Some(admin),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}

/// A migration at step dns, whose new server has address `NEW`.
async fn at_dns_step(app: &TestApp, ws: Id, skip: Value) -> Id {
    let target = Id::new();
    sqlx::query(
        "INSERT INTO provisioned_servers (id, workspace_id, provider, provider_id, name, location,
                                          server_type, monthly_price_cents, status, ipv4)
         VALUES ($1, $2, 'hetzner', '77', 'new', 'hetzner:hel1', 'cx22', 451, 'ready', $3)",
    )
    .bind(target)
    .bind(ws)
    .bind(NEW)
    .execute(app.db())
    .await
    .unwrap();
    let id = Id::new();
    sqlx::query(
        "INSERT INTO server_migrations (id, workspace_id, source_name, target_location, target_server_type,
                                        target_server_id, status, step, steps, source_ip, dns_skip)
         VALUES ($1, $2, 'old', 'hetzner:hel1', 'cx22', $3, 'running', 'dns',
                 '[{\"label\": \"DNS\", \"status\": \"running\"}]', $4, $5)",
    )
    .bind(id)
    .bind(ws)
    .bind(target)
    .bind(OLD)
    .bind(skip)
    .execute(app.db())
    .await
    .unwrap();
    id
}

async fn poll_dns(app: &TestApp, id: Id) -> bool {
    sqlx::query("UPDATE server_migrations SET dns_next_at = NULL WHERE id = $1")
        .bind(id)
        .execute(app.db())
        .await
        .unwrap();
    modula_server::migrate::server::poll(&app.state, id)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_migration_repoints_exactly_the_records_on_the_old_address(pool: PgPool) {
    let (app, cf) = app_with_cf(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;
    // More than a page of other records, so the listing must be followed.
    for i in 0..120 {
        seed(
            &cf,
            "zid1",
            &format!("f{i}"),
            &format!("f{i}.example.com"),
            "198.51.100.77",
            None,
        );
    }
    seed(
        &cf,
        "zid1",
        "theirs",
        "shop.example.com",
        OLD,
        Some("keep me"),
    );
    seed(
        &cf,
        "zid1",
        "wild",
        "*.apps.example.com",
        OLD,
        Some("Managed by Modula Deploy"),
    );
    seed(&cf, "zid1", "skipped", "skip.example.com", OLD, None);
    seed(
        &cf,
        "zid1",
        "other",
        "other.example.com",
        "198.51.100.9",
        None,
    );
    seed(&cf, "zid2", "secondzone", "a.shop.dev", OLD, None);
    // The custom-domain resync had already written the new address here.
    seed(&cf, "zid1", "dup-old", "dup.example.com", OLD, None);
    seed(&cf, "zid1", "dup-new", "dup.example.com", NEW, None);

    let id = at_dns_step(&app, ws, json!(["skipped"])).await;
    cf.ignore_writes.store(true, Ordering::SeqCst);
    let mut settled = false;
    for _ in 0..modula_server::migrate::dns::ATTEMPTS {
        settled = poll_dns(&app, id).await;
    }
    assert!(settled);
    let (status, step, error): (String, String, String) =
        sqlx::query_as("SELECT status, step, error FROM server_migrations WHERE id = $1")
            .bind(id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!((status.as_str(), step.as_str()), ("failed", "dns"));
    assert!(error.contains("only DNS is left"), "{error}");
    assert!(
        error.contains("still has"),
        "read back from Cloudflare: {error}"
    );

    cf.ignore_writes.store(false, Ordering::SeqCst);
    let r = app
        .call(
            Method::POST,
            &format!("/api/server-migrations/{id}/retry-dns"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "running");
    assert!(poll_dns(&app, id).await);
    let m = app
        .call(
            Method::GET,
            &format!("/api/server-migrations/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(m.json["status"], "done", "{:#}", m.json);

    // In place: address changed, the rest kept.
    let r = content_of(&cf, "theirs").unwrap();
    assert_eq!(r["content"], NEW);
    assert_eq!(
        (r["proxied"].clone(), r["ttl"].clone()),
        (json!(true), json!(300))
    );
    assert_eq!(r["comment"], "keep me");
    assert_eq!(content_of(&cf, "wild").unwrap()["content"], NEW);
    assert_eq!(
        content_of(&cf, "secondzone").unwrap()["content"],
        NEW,
        "paged zone"
    );
    assert_eq!(
        content_of(&cf, "skipped").unwrap()["content"],
        OLD,
        "opted out"
    );
    assert_eq!(content_of(&cf, "other").unwrap()["content"], "198.51.100.9");
    assert_eq!(content_of(&cf, "f100").unwrap()["content"], "198.51.100.77");
    assert!(content_of(&cf, "dup-old").is_none(), "no identical twin");
    assert_eq!(content_of(&cf, "dup-new").unwrap()["content"], NEW);

    let audit: Value =
        sqlx::query_scalar("SELECT metadata FROM audit_log WHERE action = 'server.migrate.dns'")
            .fetch_one(app.db())
            .await
            .unwrap();
    let changed: Vec<&str> = audit["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(changed.len(), 4, "{changed:?}");
    assert!(changed.contains(&"shop.example.com"));
    assert!(!changed.contains(&"skip.example.com"));

    // Running it again changes nothing.
    let writes = cf.writes();
    let id2 = at_dns_step_again(&app, ws).await;
    assert!(poll_dns(&app, id2).await);
    assert_eq!(cf.writes(), writes);
}

async fn at_dns_step_again(app: &TestApp, ws: Id) -> Id {
    sqlx::query("DELETE FROM provisioned_servers WHERE provider_id = '77'")
        .execute(app.db())
        .await
        .unwrap();
    at_dns_step(app, ws, json!(["skipped"])).await
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn no_dns_provider_does_not_hold_a_migration_up(pool: PgPool) {
    let (app, cf) = app_with_cf(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let id = at_dns_step(&app, ws, json!([])).await;
    assert!(poll_dns(&app, id).await);
    let status: String = sqlx::query_scalar("SELECT status FROM server_migrations WHERE id = $1")
        .bind(id)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(status, "done");
    assert_eq!(cf.calls.lock().unwrap().len(), 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn repoint_dns_moves_the_main_servers_names(pool: PgPool) {
    let (base, cf) = mock_cloudflare().await;
    let mut cfg = config();
    cfg.cloudflare_api = base;
    cfg.public_url = "https://deploy.example.com".parse().unwrap();
    cfg.apps_domain = Some("apps.example.com".into());
    cfg.tcp_proxy_host = Some("tcp.example.com".into());
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;
    // The user's own record, Modula's on another address, and one opted out.
    seed(&cf, "zid1", "dash", "deploy.example.com", OLD, Some("mine"));
    seed(
        &cf,
        "zid1",
        "wild",
        "*.apps.example.com",
        OLD,
        Some("Managed by Modula Deploy"),
    );
    seed(
        &cf,
        "zid1",
        "tcp",
        "tcp.example.com",
        "198.51.100.50",
        Some("Managed by Modula Deploy"),
    );

    let st = app.state.clone();
    let out = modula_server::migrate::promote::repoint_dns(&st, Some(OLD), NEW, &[])
        .await
        .unwrap();
    assert_eq!(out.pointed.len(), 3, "{out:?}");
    assert!(out.manual.is_empty(), "{out:?}");
    let dash = content_of(&cf, "dash").unwrap();
    assert_eq!(dash["content"], NEW);
    assert_eq!(dash["comment"], "mine", "updated in place");
    assert_eq!(content_of(&cf, "wild").unwrap()["content"], NEW);
    assert!(
        cf.records()
            .iter()
            .any(|r| r["name"] == "tcp.example.com" && r["content"] == NEW),
        "the TCP proxy host follows"
    );

    // A re-run (the retried step) has nothing to do and still knows them.
    let writes = cf.writes();
    let again = modula_server::migrate::promote::repoint_dns(&st, Some(OLD), NEW, &[])
        .await
        .unwrap();
    assert_eq!(again.pointed.len(), 3, "{again:?}");
    assert_eq!(cf.writes(), writes, "idempotent");
    // Read back: Cloudflare accepting a write isn't enough.
    cf.ignore_writes.store(true, Ordering::SeqCst);
    let third = "198.51.100.3";
    assert!(
        modula_server::migrate::promote::repoint_dns(&st, Some(NEW), third, &[])
            .await
            .is_err(),
        "a write that didn't take is a failure"
    );
}
