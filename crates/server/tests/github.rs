//! GitHub App: the manifest flow's single-use state, webhook authentication
//! and replay protection, push-to-deploy matching, pull request previews and
//! waiting for CI, against a mock GitHub.

mod common;

use axum::http::{Method, Request, StatusCode, header};
use common::*;
use hmac::{Hmac, KeyInit, Mac};
use modula_id::Id;
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::PgPool;

const WEBHOOK_SECRET: &str = "whsec-test";
const INSTALLATION: i64 = 4242;
const SHA: &str = "89abcdef0123456789abcdef0123456789abcdef";

/// A throwaway RSA key standing in for the App's private key (tests only).
const TEST_KEY: &str = include_str!("fixtures/github-app-test-key.pem");
const TEST_PUB: &str = include_str!("fixtures/github-app-test-key.pub.pem");
const INSTALL_TOKEN: &str = "ghs_testinstallationtoken";
/// The OAuth code GitHub hands back after installation (OAuth on install),
/// and the user token it exchanges for.
const USER_CODE: &str = "usercode";
const USER_TOKEN: &str = "ghu_testusertoken";

/// Requests carrying a valid App JWT (RS256 under the test key, iss = app id).
fn app_jwt_ok(h: &axum::http::HeaderMap) -> bool {
    let Some(jwt) = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    let key = jsonwebtoken::DecodingKey::from_rsa_pem(TEST_PUB.as_bytes()).unwrap();
    let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
    v.set_issuer(&["1001"]);
    v.set_required_spec_claims(&["exp", "iss"]);
    jsonwebtoken::decode::<Value>(jwt, &key, &v).is_ok()
}

fn installation_auth_ok(h: &axum::http::HeaderMap) -> bool {
    let auth = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    auth == Some(&format!("Bearer {INSTALL_TOKEN}"))
}

/// `/installation/repositories`: 120 repositories over two pages, so the
/// server has to follow pagination. `acme/secret` was pushed most recently.
fn repo_page(page: usize) -> Value {
    let all: Vec<Value> = (0..120)
        .map(|i| {
            let (name, pushed) = match i {
                0 => ("secret".to_owned(), "2026-10-01T00:00:00Z".to_owned()),
                1 => ("web-app".to_owned(), "2026-09-01T00:00:00Z".to_owned()),
                _ => (format!("lib-{i:03}"), format!("2025-01-01T00:{:02}:00Z", i % 60)),
            };
            json!({
                "full_name": format!("acme/{name}"), "private": i == 0, "default_branch": if i == 1 { "trunk" } else { "main" },
                "pushed_at": pushed, "updated_at": "2024-01-01T00:00:00Z",
                "owner": { "login": "acme", "avatar_url": "https://avatars.githubusercontent.com/u/1?v=4" }
            })
        })
        .collect();
    let chunk: Vec<Value> = all.into_iter().skip((page - 1) * 100).take(100).collect();
    json!({ "total_count": 120, "repositories": chunk })
}

type Counter = std::sync::Arc<std::sync::atomic::AtomicUsize>;

async fn mock_github() -> url::Url {
    mock_github_counted().await.0
}

async fn mock_github_counted() -> (url::Url, Counter) {
    let (url, calls, _) = mock_github_full().await;
    (url, calls)
}

/// What the mock GitHub keeps for pull requests and CI, which tests read
/// and set.
#[derive(Default)]
struct Gh {
    /// Issue comments: (id, pull request, body).
    comments: std::sync::Mutex<Vec<(i64, i64, String)>>,
    /// `check_suites` and `statuses` reported for every commit.
    suites: std::sync::Mutex<Value>,
    statuses: std::sync::Mutex<Value>,
    /// The files every pull request changes.
    files: std::sync::Mutex<Vec<String>>,
}

/// GitHub's App endpoints: manifest conversion, installations, tokens, repos,
/// branches, the App itself, pull request comments and files, check suites
/// and statuses. The counter tracks calls to the listing endpoints.
async fn mock_github_full() -> (url::Url, Counter, std::sync::Arc<Gh>) {
    use axum::extract::{Path, Query};
    use axum::routing::patch;
    use axum::routing::{get, post};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    let calls = Counter::default();
    let (c1, c2) = (calls.clone(), calls.clone());
    let gh = std::sync::Arc::new(Gh {
        suites: std::sync::Mutex::new(json!([])),
        statuses: std::sync::Mutex::new(json!([])),
        ..Default::default()
    });
    let (g1, g2, g3, g4, g5, g6) = (
        gh.clone(),
        gh.clone(),
        gh.clone(),
        gh.clone(),
        gh.clone(),
        gh.clone(),
    );
    let unauthorized = || (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
    let app = axum::Router::new()
        // Public App lookup: another instance already has the plain name.
        .route(
            "/apps/{slug}",
            get(|Path(slug): Path<String>| async move {
                if slug == "modula-deploy-cloud-test" {
                    (StatusCode::OK, axum::Json(json!({ "slug": slug })))
                } else {
                    (StatusCode::NOT_FOUND, axum::Json(json!({})))
                }
            }),
        )
        .route(
            "/app-manifests/{code}/conversions",
            post(|Path(code): Path<String>| async move {
                if code != "goodcode" {
                    return (StatusCode::NOT_FOUND, axum::Json(json!({})));
                }
                (
                    StatusCode::CREATED,
                    axum::Json(json!({
                        "id": 1001, "slug": "modula-deploy-test", "client_id": "Iv1.abc",
                        "client_secret": "cs", "webhook_secret": WEBHOOK_SECRET, "pem": TEST_KEY,
                        "html_url": "https://github.com/apps/modula-deploy-test",
                        "owner": { "login": "acme" }
                    })),
                )
            }),
        )
        .route(
            "/app/installations/{id}",
            get(move |Path(id): Path<i64>, h: axum::http::HeaderMap| async move {
                if !app_jwt_ok(&h) {
                    return unauthorized();
                }
                if id != INSTALLATION {
                    return (StatusCode::NOT_FOUND, axum::Json(json!({})));
                }
                let account = json!({ "login": "acme", "type": "Organization" });
                (StatusCode::OK, axum::Json(json!({ "id": id, "account": account })))
            }),
        )
        // OAuth on install: the code becomes a user token (client id and
        // secret from the manifest conversion)...
        .route(
            "/login/oauth/access_token",
            post(|h: axum::http::HeaderMap, axum::Json(b): axum::Json<Value>| async move {
                let json = h.get(header::ACCEPT).and_then(|v| v.to_str().ok()) == Some("application/json");
                if json && b == json!({ "client_id": "Iv1.abc", "client_secret": "cs", "code": USER_CODE }) {
                    axum::Json(json!({ "access_token": USER_TOKEN, "token_type": "bearer", "scope": "" }))
                } else {
                    axum::Json(json!({ "error": "bad_verification_code" }))
                }
            }),
        )
        // ...which lists the installations that user can access.
        .route(
            "/user/installations",
            get(|h: axum::http::HeaderMap| async move {
                let auth = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
                if auth != Some(&format!("Bearer {USER_TOKEN}")) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let one = json!({ "id": INSTALLATION, "account": { "login": "acme", "type": "Organization" } });
                (StatusCode::OK, axum::Json(json!({ "total_count": 1, "installations": [one] })))
            }),
        )
        .route(
            "/app/installations/{id}/access_tokens",
            post(move |h: axum::http::HeaderMap| async move {
                if !app_jwt_ok(&h) {
                    return unauthorized();
                }
                let expires = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
                let expires = expires.format(&time::format_description::well_known::Rfc3339).unwrap();
                (StatusCode::CREATED, axum::Json(json!({ "token": INSTALL_TOKEN, "expires_at": expires })))
            }),
        )
        .route(
            "/installation/repositories",
            get(move |h: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                if !installation_auth_ok(&h) {
                    return unauthorized();
                }
                c1.fetch_add(1, Ordering::SeqCst);
                let page = q.get("page").and_then(|p| p.parse().ok()).unwrap_or(1);
                (StatusCode::OK, axum::Json(repo_page(page)))
            }),
        )
        .route(
            "/repos/{owner}/{name}/branches",
            get(move |h: axum::http::HeaderMap, Path((owner, name)): Path<(String, String)>| async move {
                if !installation_auth_ok(&h) {
                    return unauthorized();
                }
                if (owner.as_str(), name.as_str()) != ("acme", "secret") {
                    return (StatusCode::NOT_FOUND, axum::Json(json!({})));
                }
                c2.fetch_add(1, Ordering::SeqCst);
                let branches = json!([
                    { "name": "main", "protected": true, "commit": { "sha": SHA } },
                    { "name": "feature/login", "protected": false, "commit": { "sha": SHA } },
                    { "name": "release/1.2", "protected": false, "commit": { "sha": SHA } },
                ]);
                (StatusCode::OK, axum::Json(branches))
            }),
        )
        // An App created before previews: push only, no pull requests or checks.
        .route(
            "/app",
            get(|h: axum::http::HeaderMap| async move {
                if !app_jwt_ok(&h) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let permissions = json!({ "contents": "read", "metadata": "read", "statuses": "write" });
                (StatusCode::OK, axum::Json(json!({ "id": 1001, "permissions": permissions, "events": ["push"] })))
            }),
        )
        .route(
            "/repos/{owner}/{name}/issues/{number}/comments",
            get(move |h: axum::http::HeaderMap, Path((_, _, n)): Path<(String, String, i64)>| async move {
                if !installation_auth_ok(&h) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let list: Vec<Value> = g1.comments.lock().unwrap().iter()
                    .filter(|c| c.1 == n)
                    .map(|c| json!({ "id": c.0, "body": c.2, "user": { "login": "modula[bot]", "type": "Bot" } }))
                    .collect();
                (StatusCode::OK, axum::Json(json!(list)))
            })
            .post(move |h: axum::http::HeaderMap, Path((_, _, n)): Path<(String, String, i64)>, axum::Json(b): axum::Json<Value>| async move {
                if !installation_auth_ok(&h) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let mut c = g2.comments.lock().unwrap();
                let id = 500 + c.len() as i64;
                c.push((id, n, b["body"].as_str().unwrap().to_owned()));
                (StatusCode::CREATED, axum::Json(json!({ "id": id })))
            }),
        )
        .route(
            "/repos/{owner}/{name}/issues/comments/{id}",
            patch(move |h: axum::http::HeaderMap, Path((_, _, id)): Path<(String, String, i64)>, axum::Json(b): axum::Json<Value>| async move {
                if !installation_auth_ok(&h) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let mut c = g3.comments.lock().unwrap();
                match c.iter_mut().find(|c| c.0 == id) {
                    Some(c) => {
                        c.2 = b["body"].as_str().unwrap().to_owned();
                        (StatusCode::OK, axum::Json(json!({ "id": id })))
                    }
                    None => (StatusCode::NOT_FOUND, axum::Json(json!({}))),
                }
            }),
        )
        .route(
            "/repos/{owner}/{name}/pulls/{number}/files",
            get(move |h: axum::http::HeaderMap| async move {
                if !installation_auth_ok(&h) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let files: Vec<Value> = g4.files.lock().unwrap().iter().map(|f| json!({ "filename": f })).collect();
                (StatusCode::OK, axum::Json(json!(files)))
            }),
        )
        .route(
            "/repos/{owner}/{name}/commits/{sha}/check-suites",
            get(move |h: axum::http::HeaderMap| async move {
                if !installation_auth_ok(&h) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let suites = g5.suites.lock().unwrap().clone();
                (StatusCode::OK, axum::Json(json!({ "total_count": suites.as_array().unwrap().len(), "check_suites": suites })))
            }),
        )
        .route(
            "/repos/{owner}/{name}/commits/{sha}/status",
            get(move |h: axum::http::HeaderMap| async move {
                if !installation_auth_ok(&h) {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
                }
                let statuses = g6.statuses.lock().unwrap().clone();
                (StatusCode::OK, axum::Json(json!({ "state": "pending", "statuses": statuses })))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}").parse().unwrap(), calls, gh)
}

async fn app_with_github(pool: PgPool) -> (TestApp, String) {
    let mut cfg = config();
    cfg.github_api = mock_github().await;
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    (app, admin)
}

async fn manifest_state(app: &TestApp, admin: &str) -> String {
    let r = app
        .call(
            Method::POST,
            "/api/github/app/manifest",
            Some(admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        r.json["manifest"]["hook_attributes"]["url"],
        format!("{ORIGIN}/api/github/webhook")
    );
    // Public: installs on any account, picked on GitHub's installation page.
    assert_eq!(r.json["manifest"]["public"], true);
    r.json["state"].as_str().unwrap().to_owned()
}

/// The `Location` of a redirect.
fn location(r: &Resp) -> String {
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{:?}", r.json);
    r.headers[header::LOCATION].to_str().unwrap().to_owned()
}

fn state_param(url: &str) -> String {
    let url: url::Url = url.parse().unwrap();
    url.query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned()
}

async fn install_callback(app: &TestApp, cookie: &str, query: &str) -> Resp {
    app.call(
        Method::GET,
        &format!("/api/github/install/callback?{query}"),
        Some(cookie),
        None,
    )
    .await
}

async fn callback(app: &TestApp, cookie: &str, code: &str, state: &str) -> Resp {
    app.call(
        Method::GET,
        &format!("/api/github/app/callback?code={code}&state={state}"),
        Some(cookie),
        None,
    )
    .await
}

async fn create_app(app: &TestApp, admin: &str) {
    let st = manifest_state(app, admin).await;
    let r = callback(app, admin, "goodcode", &st).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{:?}", r.json);
}

fn sign(body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// A delivery as GitHub sends it: no session, no Origin.
async fn deliver(
    app: &TestApp,
    event: &str,
    delivery: &str,
    body: &Value,
    sig: Option<String>,
) -> Resp {
    let mut b = Request::builder()
        .method(Method::POST)
        .uri("/api/github/webhook")
        .header("x-github-event", event)
        .header("x-github-delivery", delivery);
    if let Some(s) = sig {
        b = b.header("x-hub-signature-256", s);
    }
    app.send(b, None, Some(body.clone())).await
}

async fn signed(app: &TestApp, event: &str, body: Value) -> Resp {
    let sig = sign(body.to_string().as_bytes());
    deliver(app, event, &Id::new().to_string(), &body, Some(sig)).await
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn manifest_callback_state_is_single_use_and_bound(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    let (_, other) = app.user("other@example.com").await;

    // A made-up state, and a real one presented by someone else (which burns it).
    let fake = format!("mcgh_{}", "A".repeat(43));
    assert_eq!(
        callback(&app, &admin, "goodcode", &fake).await.status,
        StatusCode::BAD_REQUEST
    );
    let st = manifest_state(&app, &admin).await;
    assert_eq!(
        callback(&app, &other, "goodcode", &st).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        callback(&app, &admin, "goodcode", &st).await.status,
        StatusCode::BAD_REQUEST
    );

    // The right admin completes it once; the state can't be replayed.
    let st = manifest_state(&app, &admin).await;
    assert_eq!(
        callback(&app, &admin, "goodcode", &st).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        callback(&app, &admin, "goodcode", &st).await.status,
        StatusCode::BAD_REQUEST
    );

    // Credentials are stored sealed, never in the clear.
    let (slug, sealed): (String, Vec<u8>) =
        sqlx::query_as("SELECT slug, webhook_secret_sealed FROM github_apps")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(slug, "modula-deploy-test");
    assert!(
        !sealed
            .windows(WEBHOOK_SECRET.len())
            .any(|w| w == WEBHOOK_SECRET.as_bytes())
    );
    // Recorded as public, and one public App is all an instance needs.
    let public: bool = sqlx::query_scalar("SELECT public FROM github_apps")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert!(public);
    let r = app
        .call(
            Method::POST,
            "/api/github/app/manifest",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    // One App per account: the owner already has one...
    let r = app
        .call(
            Method::POST,
            "/api/github/app/manifest",
            Some(&admin),
            Some(json!({ "org": "ACME" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    // ...another organization gets its own.
    let r = app
        .call(
            Method::POST,
            "/api/github/app/manifest",
            Some(&admin),
            Some(json!({ "org": "other-org" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // The same App coming back again is refused.
    let st = r.json["state"].as_str().unwrap().to_owned();
    assert_eq!(
        callback(&app, &admin, "goodcode", &st).await.status,
        StatusCode::CONFLICT
    );
}

/// "Connect GitHub" on an instance without an App: creating it carries
/// straight on to installing it, and the user ends up where they started.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn connect_creates_installs_and_returns(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    let ws = app.create_workspace(&admin, "team").await;
    let back = "/projects/p1?env=e1&add=repo";
    let r = app
        .call(
            Method::POST,
            "/api/github/app/manifest",
            Some(&admin),
            Some(json!({ "workspaceId": ws, "returnTo": back })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // Least privilege, push events, and a fresh name (the plain one is taken).
    let m = &r.json["manifest"];
    assert_eq!(
        m["default_permissions"],
        json!({
            "checks": "read", "contents": "read", "issues": "write", "metadata": "read",
            "pull_requests": "read", "statuses": "write",
        })
    );
    assert_eq!(
        m["default_events"],
        json!(["check_suite", "pull_request", "push", "status"])
    );
    // The installing user authorizes on the way back (see install_callback).
    assert_eq!(m["request_oauth_on_install"], true);
    assert_eq!(
        m["callback_urls"],
        json!([format!("{ORIGIN}/api/github/install/callback")])
    );
    let name = m["name"].as_str().unwrap();
    let suffix = name.strip_prefix("Modula Deploy (cloud.test) ").unwrap();
    assert!(suffix.len() == 4 && suffix.bytes().all(|b| b.is_ascii_hexdigit()));
    let st = r.json["state"].as_str().unwrap().to_owned();

    // The manifest callback goes straight on to GitHub's installation page.
    let to = location(&callback(&app, &admin, "goodcode", &st).await);
    assert!(
        to.starts_with(
            "https://github.com/apps/modula-deploy-test/installations/select_target?state=mcgh_"
        ),
        "{to}"
    );
    let st = state_param(&to);

    // And the installation lands back where the user started.
    let r = install_callback(
        &app,
        &admin,
        &format!("installation_id={INSTALLATION}&setup_action=install&state={st}&code={USER_CODE}"),
    )
    .await;
    assert_eq!(location(&r), format!("{back}&github=installed"));
    let bound: Id =
        sqlx::query_scalar("SELECT workspace_id FROM github_installations WHERE id = $1")
            .bind(INSTALLATION)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(bound, ws);

    // With the App in place, Connect goes straight to the installation page.
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/github/install"),
            Some(&admin),
            Some(json!({ "returnTo": "/settings#integrations-github" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let st = state_param(r.json["url"].as_str().unwrap());
    // An organization owner has to approve: say so, back where they started.
    let r = install_callback(&app, &admin, &format!("setup_action=request&state={st}")).await;
    assert_eq!(
        location(&r),
        "/settings?github=requested#integrations-github"
    );
}

/// States are single-use, expire, can't be tampered with, and only send the
/// browser back to this site; only instance admins create the App.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn connect_states_and_permissions(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    let ws = app.create_workspace(&admin, "team").await;
    let manifest = |cookie: String, body: Value| {
        let app = &app;
        async move {
            app.call(
                Method::POST,
                "/api/github/app/manifest",
                Some(&cookie),
                Some(body),
            )
            .await
        }
    };

    // Workspace admins who aren't instance admins can't create the App.
    let (member_id, member) = app.user("wsadmin@example.com").await;
    app.add_member(ws, member_id, "admin").await;
    let r = manifest(member.clone(), json!({ "workspaceId": ws })).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    // Nor can an instance admin target a workspace they don't administer.
    let elsewhere = app.create_workspace(&member, "elsewhere").await;
    let r = manifest(admin.clone(), json!({ "workspaceId": elsewhere })).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);

    // Only paths on this site to come back to.
    for evil in [
        "//evil.example",
        "https://evil.example/",
        "/\\evil.example",
        "settings",
    ] {
        let r = manifest(
            admin.clone(),
            json!({ "workspaceId": ws, "returnTo": evil }),
        )
        .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{evil}: {:?}", r.json);
    }

    // A tampered state is rejected, and so is an expired one.
    let r = manifest(
        admin.clone(),
        json!({ "workspaceId": ws, "returnTo": "/settings" }),
    )
    .await;
    let st = r.json["state"].as_str().unwrap().to_owned();
    let mut tampered = st.clone().into_bytes();
    let last = tampered.last_mut().unwrap();
    *last = if *last == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).unwrap();
    assert_eq!(
        callback(&app, &admin, "goodcode", &tampered).await.status,
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE github_flow_states SET expires_at = now() - interval '1 second'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(
        callback(&app, &admin, "goodcode", &st).await.status,
        StatusCode::BAD_REQUEST
    );
    let apps: i64 = sqlx::query_scalar("SELECT count(*) FROM github_apps")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(apps, 0);

    // The same holds for the installation leg.
    create_app(&app, &admin).await;
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/github/install"),
            Some(&admin),
            Some(json!({ "returnTo": "/settings" })),
        )
        .await;
    let st = state_param(r.json["url"].as_str().unwrap());
    sqlx::query("UPDATE github_flow_states SET expires_at = now() - interval '1 second'")
        .execute(app.db())
        .await
        .unwrap();
    let r = install_callback(
        &app,
        &admin,
        &format!("installation_id={INSTALLATION}&setup_action=install&state={st}&code={USER_CODE}"),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let installs: i64 = sqlx::query_scalar("SELECT count(*) FROM github_installations")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(installs, 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn webhook_requires_a_valid_signature_and_ignores_replays(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    let ping = json!({ "zen": "Keep it logically awesome." });
    let good = Some(sign(ping.to_string().as_bytes()));

    // No App yet: nothing can be verified.
    assert_eq!(
        deliver(&app, "ping", "d-0", &ping, good.clone())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    create_app(&app, &admin).await;

    assert_eq!(
        deliver(&app, "ping", "d-1", &ping, None).await.status,
        StatusCode::UNAUTHORIZED
    );
    let forged = Some(format!("sha256={}", "00".repeat(32)));
    assert_eq!(
        deliver(&app, "ping", "d-1", &ping, forged).await.status,
        StatusCode::UNAUTHORIZED
    );
    let tampered = json!({ "zen": "Something else." });
    assert_eq!(
        deliver(&app, "ping", "d-1", &tampered, good.clone())
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let r = deliver(&app, "ping", "d-1", &ping, good.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["duplicate"], Value::Null);
    let r = deliver(&app, "ping", "d-1", &ping, good).await;
    assert_eq!(r.json["duplicate"], true);

    // The App is public: a stranger can install it and have GitHub send us
    // validly signed events. An installation no workspace bound is dropped
    // without a trace.
    let stranger = push("refs/heads/main", 4242, &["x"]);
    let sig = Some(sign(stranger.to_string().as_bytes()));
    let r = deliver(&app, "push", "d-2", &stranger, sig).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.json);
    assert_eq!(r.json["ignored"], true);
    let created = json!({ "action": "created", "installation": { "id": 4242 } });
    let sig = Some(sign(created.to_string().as_bytes()));
    let r = deliver(&app, "installation", "d-3", &created, sig).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.json);
    let recorded: i64 =
        sqlx::query_scalar("SELECT count(*) FROM github_deliveries WHERE id IN ('d-2', 'd-3')")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(recorded, 0);

    // Only that exact path skips the Origin check.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/api/github/webhookx")
        .header(header::CONTENT_TYPE, "application/json");
    assert_eq!(
        app.send(req, None, Some(json!({}))).await.status,
        StatusCode::FORBIDDEN
    );
}

/// Workspace with an installation for `acme`, a node, and repo instances.
async fn seed(app: &TestApp, admin: &str, instances: &[(&str, Value)]) -> Vec<Id> {
    let ws = app.create_workspace(admin, "team").await;
    let db = app.db();
    let (project, env, node) = (Id::new(), Id::new(), Id::new());
    sqlx::query("INSERT INTO projects (id, workspace_id, name) VALUES ($1, $2, 'p')")
        .bind(project)
        .bind(ws)
        .execute(db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO environments (id, project_id, name) VALUES ($1, $2, 'production')")
        .bind(env)
        .bind(project)
        .execute(db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO nodes (id, name, cert_serial, cert_expires_at)
         VALUES ($1, 'n', $1::text, now() + interval '1 day')",
    )
    .bind(node)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO github_installations (id, app_id, workspace_id, account_login, account_type)
         VALUES ($1, 1001, $2, 'acme', 'Organization')",
    )
    .bind(INSTALLATION)
    .bind(ws)
    .execute(db)
    .await
    .unwrap();
    let mut ids = Vec::new();
    for (name, config) in instances {
        let (service, instance) = (Id::new(), Id::new());
        sqlx::query("INSERT INTO services (id, project_id, name) VALUES ($1, $2, $3)")
            .bind(service)
            .bind(project)
            .bind(name)
            .execute(db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO service_instances (id, service_id, environment_id, node_id, config)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(instance)
        .bind(service)
        .bind(env)
        .bind(node)
        .bind(config)
        .execute(db)
        .await
        .unwrap();
        ids.push(instance);
    }
    ids
}

fn push(git_ref: &str, installation: i64, files: &[&str]) -> Value {
    json!({
        "ref": git_ref,
        "before": "0000000000000000000000000000000000000000",
        "after": SHA,
        "deleted": false,
        "repository": { "full_name": "acme/App" },
        "installation": { "id": installation },
        "head_commit": { "id": SHA, "message": "Fix checkout\n\nLonger body", "author": { "name": "Ada" } },
        "commits": [{ "added": [], "modified": files, "removed": [] }]
    })
}

async fn deployments(app: &TestApp) -> Vec<(Id, String, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT service_instance_id, trigger, commit_sha, commit_message FROM deployments",
    )
    .fetch_all(app.db())
    .await
    .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn signed_push_deploys_only_matching_services(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    create_app(&app, &admin).await;
    let repo = |branch: &str| json!({ "type": "repo", "repo": "acme/app", "branch": branch });
    let ids = seed(
        &app,
        &admin,
        &[
            (
                "api",
                json!({ "source": repo("main"), "watchPaths": ["services/api/"] }),
            ),
            (
                "manual",
                json!({ "source": repo("main"), "autoDeploy": false }),
            ),
            ("staging", json!({ "source": repo("staging") })),
        ],
    )
    .await;

    // Filtered out: other paths, another branch, a tag, an unknown installation.
    for body in [
        push("refs/heads/main", INSTALLATION, &["docs/README.md"]),
        push(
            "refs/heads/feature",
            INSTALLATION,
            &["services/api/main.go"],
        ),
        push("refs/tags/v1", INSTALLATION, &["services/api/main.go"]),
        push("refs/heads/main", 999, &["services/api/main.go"]),
    ] {
        let unknown = body["installation"]["id"] == 999;
        let r = signed(&app, "push", body).await;
        // An installation no workspace bound is dropped up front.
        let want = if unknown {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        };
        assert_eq!(r.status, want, "{:?}", r.json);
    }
    assert!(deployments(&app).await.is_empty());

    let r = signed(
        &app,
        "push",
        push("refs/heads/main", INSTALLATION, &["services/api/main.go"]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["deployments"].as_array().unwrap().len(), 1);
    assert_eq!(
        deployments(&app).await,
        [(
            ids[0],
            "push".into(),
            Some(SHA.into()),
            Some("Fix checkout".into())
        )]
    );

    // Build progress is queued for reporting on the commit, never sent inline.
    let (dep, node): (Id, Id) = sqlx::query_as("SELECT id, node_id FROM deployments")
        .fetch_one(app.db())
        .await
        .unwrap();
    let ev = modula_proto::node::v1::DeploymentEvent {
        deployment_id: dep.to_string(),
        phase: modula_proto::node::v1::DeploymentPhase::Building as i32,
        ..Default::default()
    };
    modula_server::deploy::state::apply_event(app.db(), node, &ev)
        .await
        .unwrap();
    let jobs: Vec<Value> =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'github.commit_status'")
            .fetch_all(app.db())
            .await
            .unwrap();
    assert_eq!(jobs, [json!({ "deploymentId": dep, "state": "pending" })]);

    // Unrelated events are acknowledged and ignored.
    let r = signed(&app, "issues", json!({ "action": "opened" })).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    // Uninstalling stops deploys from that installation.
    let r = signed(
        &app,
        "installation",
        json!({ "action": "deleted", "installation": { "id": INSTALLATION } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    signed(
        &app,
        "push",
        push("refs/heads/staging", INSTALLATION, &["x"]),
    )
    .await;
    assert_eq!(deployments(&app).await.len(), 1);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn installation_binds_to_the_workspace_that_started_it(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    create_app(&app, &admin).await;
    let ws = app.create_workspace(&admin, "team").await;
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/github/install"),
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let url: url::Url = r.json["url"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        url.path(),
        "/apps/modula-deploy-test/installations/select_target"
    );
    let st = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();

    // The callback verifies the installation with an App JWT (checked by the mock).
    let cb = |id: i64, st: &str| {
        format!(
            "/api/github/install/callback?installation_id={id}&setup_action=install&state={st}&code={USER_CODE}"
        )
    };
    let r = app
        .call(Method::GET, &cb(INSTALLATION, &st), Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{:?}", r.json);
    let (bound, login): (Id, String) = sqlx::query_as(
        "SELECT workspace_id, account_login FROM github_installations WHERE id = $1",
    )
    .bind(INSTALLATION)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((bound, login.as_str()), (ws, "acme"));
    assert_eq!(
        app.call(Method::GET, &cb(INSTALLATION, &st), Some(&admin), None)
            .await
            .status,
        StatusCode::BAD_REQUEST
    );

    // Another workspace can't take it over.
    let other = app.create_workspace(&admin, "other").await;
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{other}/github/install"),
            Some(&admin),
            Some(json!({})),
        )
        .await;
    let url: url::Url = r.json["url"].as_str().unwrap().parse().unwrap();
    let st = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let r = app
        .call(Method::GET, &cb(INSTALLATION, &st), Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);

    // The repo picker lists what the installation can reach (installation token).
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/github/repos?q=secret"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        r.json,
        json!([{
            "fullName": "acme/secret", "private": true, "defaultBranch": "main",
            "updatedAt": "2026-10-01T00:00:00Z",
            "ownerAvatar": "https://avatars.githubusercontent.com/u/1?v=4"
        }])
    );
    // The settings card shows the installation, with links but no secrets.
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/github/installations"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let inst = &r.json[0];
    assert_eq!(inst["id"], INSTALLATION);
    assert_eq!(inst["accountLogin"], "acme");
    assert_eq!(
        inst["configureUrl"],
        format!("https://github.com/organizations/acme/settings/installations/{INSTALLATION}")
    );
    assert_eq!(
        inst["avatarUrl"],
        "https://avatars.githubusercontent.com/acme?s=64"
    );
    // The App's status says where webhooks go: the instance's public URL.
    let r = app
        .call(Method::GET, "/api/github/apps", Some(&admin), None)
        .await;
    assert_eq!(r.json["webhookUrl"], format!("{ORIGIN}/api/github/webhook"));
    assert_eq!(r.json["apps"][0]["ownerLogin"], "acme");
    assert_eq!(r.json["apps"][0]["id"], 1001);
    assert_eq!(inst["appId"], 1001);
    assert!(!r.json.to_string().contains(WEBHOOK_SECRET));
}

/// The repository and branch pickers: search, recency order, pagination,
/// caching, and which workspaces and roles may use them.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn repo_and_branch_pickers(pool: PgPool) {
    use std::sync::atomic::Ordering;
    let mut cfg = config();
    let (api, calls) = mock_github_counted().await;
    cfg.github_api = api;
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    create_app(&app, &admin).await;
    seed(&app, &admin, &[]).await;
    let ws: Id = sqlx::query_scalar("SELECT workspace_id FROM github_installations")
        .fetch_one(app.db())
        .await
        .unwrap();
    let get = |path: String, cookie: String| {
        let app = &app;
        async move { app.call(Method::GET, &path, Some(&cookie), None).await }
    };
    let repos = |q: &str| format!("/api/workspaces/{ws}/github/repos{q}");

    // Both pages fetched; most recently pushed first; never a token.
    let before = calls.load(Ordering::SeqCst);
    let r = get(repos("?perPage=100"), admin.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let names: Vec<&str> = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["fullName"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 100);
    assert_eq!(&names[..2], ["acme/secret", "acme/web-app"]);
    assert!(!r.json.to_string().contains(INSTALL_TOKEN));
    let r = get(repos("?perPage=100&page=2"), admin.clone()).await;
    assert_eq!(r.json.as_array().unwrap().len(), 20);
    // Search is case-insensitive, and served from the cache.
    let r = get(repos("?q=WEB"), admin.clone()).await;
    assert_eq!(r.json[0]["fullName"], "acme/web-app");
    assert_eq!(r.json[0]["defaultBranch"], "trunk");
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    assert_eq!(
        calls.load(Ordering::SeqCst) - before,
        2,
        "one listing, two pages"
    );

    // A webhook about repository access drops the cache.
    let r = signed(
        &app,
        "installation_repositories",
        json!({ "action": "added", "installation": { "id": INSTALLATION } }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    get(repos(""), admin.clone()).await;
    assert_eq!(calls.load(Ordering::SeqCst) - before, 4);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'github.install.repositories'",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // Branches of a repository the installation reaches.
    let branches =
        |repo: &str, q: &str| format!("/api/workspaces/{ws}/github/branches?repo={repo}{q}");
    let r = get(branches("acme/secret", ""), admin.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json[0], json!({ "name": "main", "protected": true }));
    let r = get(branches("acme/secret", "&perPage=1&page=2"), admin.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        r.json,
        json!([{ "name": "feature/login", "protected": false }])
    );
    let r = get(branches("acme/secret", "&q=release"), admin.clone()).await;
    assert_eq!(
        r.json,
        json!([{ "name": "release/1.2", "protected": false }])
    );
    // No installation covers this owner; malformed names are rejected.
    let r = get(branches("someone/else", ""), admin.clone()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = get(branches("acme/..", ""), admin.clone()).await;
    assert_ne!(r.status, StatusCode::OK);

    // Members may use the pickers; viewers may not.
    let (member_id, member) = app.user("member@example.com").await;
    app.add_member(ws, member_id, "member").await;
    assert_eq!(get(repos(""), member.clone()).await.status, StatusCode::OK);
    let (viewer_id, viewer) = app.user("viewer@example.com").await;
    app.add_member(ws, viewer_id, "viewer").await;
    assert_eq!(
        get(repos(""), viewer.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(branches("acme/secret", ""), viewer.clone())
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Another workspace sees none of this workspace's repositories or branches.
    let other = app.create_workspace(&member, "elsewhere").await;
    let r = get(
        format!("/api/workspaces/{other}/github/repos"),
        member.clone(),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json, json!([]));
    let r = get(
        format!("/api/workspaces/{other}/github/branches?repo=acme/secret"),
        member.clone(),
    )
    .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = get(
        format!("/api/workspaces/{other}/github/installations"),
        member.clone(),
    )
    .await;
    assert_eq!(r.json, json!([]));
    // And outsiders can't reach the workspace at all.
    let (_, outsider) = app.user("outsider@example.com").await;
    assert_ne!(get(repos(""), outsider).await.status, StatusCode::OK);
}

/// One service in two environments, each tracking its own branch (the
/// source is per environment): a push deploys only the environment that
/// tracks the pushed branch.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn each_environment_deploys_its_own_branch(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    create_app(&app, &admin).await;
    let repo = |branch: &str| json!({ "type": "repo", "repo": "acme/app", "branch": branch });
    let production = seed(&app, &admin, &[("api", json!({ "source": repo("main") }))]).await[0];
    // The same service's instance in a staging environment.
    let (project, service, node): (Id, Id, Id) = sqlx::query_as(
        "SELECT s.project_id, i.service_id, i.node_id FROM service_instances i
         JOIN services s ON s.id = i.service_id WHERE i.id = $1",
    )
    .bind(production)
    .fetch_one(app.db())
    .await
    .unwrap();
    let (env, staging) = (Id::new(), Id::new());
    sqlx::query("INSERT INTO environments (id, project_id, name) VALUES ($1, $2, 'staging')")
        .bind(env)
        .bind(project)
        .execute(app.db())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO service_instances (id, service_id, environment_id, node_id, config)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(staging)
    .bind(service)
    .bind(env)
    .bind(node)
    .bind(json!({ "source": repo("staging") }))
    .execute(app.db())
    .await
    .unwrap();

    let r = signed(
        &app,
        "push",
        push("refs/heads/staging", INSTALLATION, &["src/a.ts"]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let deployed: Vec<Id> = deployments(&app).await.iter().map(|d| d.0).collect();
    assert_eq!(deployed, [staging]);

    let r = signed(
        &app,
        "push",
        push("refs/heads/main", INSTALLATION, &["src/a.ts"]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let mut deployed: Vec<Id> = deployments(&app).await.iter().map(|d| d.0).collect();
    deployed.sort();
    let mut want = vec![production, staging];
    want.sort();
    assert_eq!(deployed, want);
}

/// The callback's `installation_id` is only a query parameter: it binds only
/// when the GitHub user coming back (OAuth on install) can access it.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn install_callback_requires_the_users_own_installation(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    create_app(&app, &admin).await;
    let ws = app.create_workspace(&admin, "team").await;
    let fresh_state = || async {
        let r = app
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/github/install"),
                Some(&admin),
                Some(json!({})),
            )
            .await;
        state_param(r.json["url"].as_str().unwrap())
    };
    let installs = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM github_installations")
            .fetch_one(app.db())
            .await
            .unwrap()
    };

    // Someone else's installation id (even a real one of this App's), a
    // missing code, and a code GitHub rejects all bind nothing.
    let st = fresh_state().await;
    let r = install_callback(
        &app,
        &admin,
        &format!("installation_id=5555&setup_action=install&state={st}&code={USER_CODE}"),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    assert_eq!(r.json["error"]["message"], "installation not found");
    let st = fresh_state().await;
    let r = install_callback(
        &app,
        &admin,
        &format!("installation_id={INSTALLATION}&setup_action=install&state={st}"),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let st = fresh_state().await;
    let r = install_callback(
        &app,
        &admin,
        &format!("installation_id={INSTALLATION}&setup_action=install&state={st}&code=stolen"),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    assert_eq!(installs().await, 0);

    // The user's own installation binds.
    let st = fresh_state().await;
    let r = install_callback(
        &app,
        &admin,
        &format!("installation_id={INSTALLATION}&setup_action=install&state={st}&code={USER_CODE}"),
    )
    .await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{:?}", r.json);
    assert_eq!(installs().await, 1);
}

/// A replayed push (new delivery id, same signed body) starts no second
/// deployment of a commit the instance already has.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn replayed_push_deploys_a_commit_once(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    create_app(&app, &admin).await;
    let repo = json!({ "type": "repo", "repo": "acme/app", "branch": "main" });
    seed(&app, &admin, &[("api", json!({ "source": repo }))]).await;
    let body = push("refs/heads/main", INSTALLATION, &["src/main.rs"]);
    let r = signed(&app, "push", body.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["deployments"].as_array().unwrap().len(), 1);
    let r = signed(&app, "push", body).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["deployments"], json!([]));
    assert_eq!(deployments(&app).await.len(), 1);
}

/// A delivery signed with `secret`, naming App `target` the way GitHub does.
async fn signed_as(
    app: &TestApp,
    secret: &str,
    target: Option<i64>,
    event: &str,
    body: &Value,
) -> Resp {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.to_string().as_bytes());
    let sig = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    let mut b = Request::builder()
        .method(Method::POST)
        .uri("/api/github/webhook")
        .header("x-github-event", event)
        .header("x-github-delivery", Id::new().to_string())
        .header("x-hub-signature-256", sig);
    if let Some(t) = target {
        b = b.header("x-github-hook-installation-target-id", t.to_string());
    }
    app.send(b, None, Some(body.clone())).await
}

/// One App per GitHub account: each App's webhooks act only on its own
/// installations, installing needs the App named once there are several,
/// and removing an App removes its installations.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn several_apps_stay_apart(pool: PgPool) {
    let (app, admin) = app_with_github(pool).await;
    create_app(&app, &admin).await;
    // A second App (another organization's), with its own webhook secret.
    const OTHER: i64 = 2002;
    const OTHER_SECRET: &str = "whsec-other";
    let row = OTHER.to_string();
    let mut sealed = Vec::new();
    for (col, v) in [
        ("private_key", b"not a key".as_slice()),
        ("webhook_secret", OTHER_SECRET.as_bytes()),
        ("client_secret", b"cs2".as_slice()),
    ] {
        let aad = modula_server::github::app::aad(col, &row);
        sealed.push(app.state.keys.seal(app.db(), v, aad).await.unwrap());
    }
    sqlx::query(
        "INSERT INTO github_apps (id, slug, client_id, html_url, owner_login, private_key_sealed,
                                  webhook_secret_sealed, client_secret_sealed)
         VALUES ($1, 'modula-deploy-other', 'Iv1.other', 'https://github.com/apps/modula-deploy-other',
                 'other-org', $2, $3, $4)",
    )
    .bind(OTHER)
    .bind(&sealed[0])
    .bind(&sealed[1])
    .bind(&sealed[2])
    .execute(app.db())
    .await
    .unwrap();
    let repo = json!({ "type": "repo", "repo": "acme/app", "branch": "main" });
    seed(&app, &admin, &[("api", json!({ "source": repo }))]).await;
    let body = push("refs/heads/main", INSTALLATION, &["x"]);

    // The other App's correctly signed push can't reach the first App's installation.
    for target in [Some(OTHER), None] {
        let r = signed_as(&app, OTHER_SECRET, target, "push", &body).await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.json);
        assert_eq!(r.json["ignored"], true);
    }
    assert!(deployments(&app).await.is_empty());
    // Claiming to be the first App with the other's secret fails.
    let r = signed_as(&app, OTHER_SECRET, Some(1001), "push", &body).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // The first App's own push deploys.
    let r = signed_as(&app, WEBHOOK_SECRET, Some(1001), "push", &body).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(deployments(&app).await.len(), 1);

    // Two Apps: installing names one.
    let ws: Id = sqlx::query_scalar("SELECT id FROM workspaces WHERE name = 'team'")
        .fetch_one(app.db())
        .await
        .unwrap();
    let path = format!("/api/workspaces/{ws}/github/install");
    let install = |b: Value| app.call(Method::POST, &path, Some(&admin), Some(b));
    assert_eq!(install(json!({})).await.status, StatusCode::CONFLICT);
    let r = install(json!({ "appId": OTHER })).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(
        r.json["url"]
            .as_str()
            .unwrap()
            .starts_with("https://github.com/apps/modula-deploy-other/")
    );
    assert_eq!(
        install(json!({ "appId": 31337 })).await.status,
        StatusCode::NOT_FOUND
    );

    // Removing the first App takes its installation with it.
    let r = app
        .call(
            Method::POST,
            "/api/github/apps/remove",
            Some(&admin),
            Some(json!({ "id": 1001 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{:?}", r.json);
    let installs: i64 = sqlx::query_scalar("SELECT count(*) FROM github_installations")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(installs, 0);
    let r = app
        .call(Method::GET, "/api/github/apps", Some(&admin), None)
        .await;
    assert_eq!(r.json["apps"].as_array().unwrap().len(), 1);
    assert_eq!(r.json["apps"][0]["ownerLogin"], "other-org");
    // Its webhooks no longer authenticate.
    let r = signed_as(&app, WEBHOOK_SECRET, Some(1001), "push", &body).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

const SHA2: &str = "1123456789abcdef0123456789abcdef01234567";
const SHA3: &str = "2123456789abcdef0123456789abcdef01234567";

/// An App (created through the manifest flow) and the mock's pull request
/// and CI state.
async fn app_with_gh(pool: PgPool) -> (TestApp, String, std::sync::Arc<Gh>) {
    let mut cfg = config();
    let (url, _, gh) = mock_github_full().await;
    cfg.github_api = url;
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    create_app(&app, &admin).await;
    (app, admin, gh)
}

/// Run the queued jobs of `kind` once, as the worker would.
async fn run_jobs(app: &TestApp, kind: &str) -> usize {
    let jobs: Vec<modula_server::jobs::Job> = sqlx::query_as(
        "DELETE FROM jobs WHERE kind = $1 AND status = 'queued'
         RETURNING id, kind, payload, attempts, max_attempts",
    )
    .bind(kind)
    .fetch_all(app.db())
    .await
    .unwrap();
    let n = jobs.len();
    for job in jobs {
        let s = app.state.clone();
        match kind {
            modula_server::github::pull_request::COMMENT_JOB => {
                modula_server::github::pull_request::comment_job(s, job)
                    .await
                    .unwrap()
            }
            modula_server::github::ci::TIMEOUT_JOB => {
                modula_server::github::ci::timeout_job(s, job)
                    .await
                    .unwrap()
            }
            other => panic!("no runner for {other}"),
        }
    }
    n
}

fn pull_request(action: &str, number: i64, sha: &str, association: &str, head: Value) -> Value {
    json!({
        "action": action,
        "number": number,
        "pull_request": {
            "title": "Add login",
            "author_association": association,
            "user": { "login": "ada" },
            "head": { "ref": "feature/login", "sha": sha, "repo": head },
        },
        "repository": { "full_name": "acme/App" },
        "installation": { "id": INSTALLATION },
    })
}

fn own_head() -> Value {
    json!({ "full_name": "acme/App" })
}

/// The project and production environment of a seeded instance.
async fn project_of(app: &TestApp, instance: Id) -> (Id, Id) {
    sqlx::query_as(
        "SELECT s.project_id, i.environment_id FROM service_instances i
         JOIN services s ON s.id = i.service_id WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(app.db())
    .await
    .unwrap()
}

async fn previews(app: &TestApp) -> Vec<(Id, String, Option<String>)> {
    sqlx::query_as(
        "SELECT id, name, pr_head_sha FROM environments WHERE ephemeral ORDER BY created_at",
    )
    .fetch_all(app.db())
    .await
    .unwrap()
}

/// Service name and commit of each deployment in `env`, by name.
async fn env_deployments(app: &TestApp, env: Id) -> Vec<(String, Option<String>)> {
    sqlx::query_as(
        "SELECT s.name, d.commit_sha FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id
         WHERE i.environment_id = $1 ORDER BY s.name, d.created_at",
    )
    .bind(env)
    .fetch_all(app.db())
    .await
    .unwrap()
}

/// A pull request gets a preview environment duplicated from production
/// (without its sealed variables), deployed from its branch, with one comment
/// that follows the deployments; closing it removes the preview.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn pull_requests_get_previews(pool: PgPool) {
    let (app, admin, gh) = app_with_gh(pool).await;
    let repo = |branch: &str| json!({ "type": "repo", "repo": "acme/app", "branch": branch });
    let ids = seed(
        &app,
        &admin,
        &[
            (
                "api",
                json!({ "source": repo("main"), "watchPaths": ["services/api/"] }),
            ),
            ("web", json!({ "source": repo("main") })),
            (
                "cache",
                json!({ "source": { "type": "image", "image": "redis:7" } }),
            ),
        ],
    )
    .await;
    let db = app.db();
    let (project, production) = project_of(&app, ids[0]).await;
    // web is public in production; api has a secret and a plain variable.
    sqlx::query(
        "INSERT INTO domains (id, service_instance_id, hostname, kind)
         VALUES ($1, $2, 'web.apps.test', 'generated')",
    )
    .bind(Id::new())
    .bind(ids[1])
    .execute(db)
    .await
    .unwrap();
    for (name, value, sealed) in [("PLAIN", "hello", false), ("SECRET", "s3cret", true)] {
        let id = Id::new();
        let v =
            modula_server::deploy::env::seal_variable(&app.state.keys, db, production, id, value)
                .await
                .unwrap();
        sqlx::query(
            "INSERT INTO variables (id, environment_id, service_instance_id, name, value_sealed, sealed, refs)
             VALUES ($1, $2, $3, $4, $5, $6, '{}')",
        )
        .bind(id)
        .bind(production)
        .bind(ids[0])
        .bind(name)
        .bind(v)
        .bind(sealed)
        .execute(db)
        .await
        .unwrap();
    }

    // Previews are off by default.
    let opened = pull_request("opened", 7, SHA, "MEMBER", own_head());
    let r = signed(&app, "pull_request", opened.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(previews(&app).await.is_empty());
    let r = app
        .call(
            Method::PATCH,
            &format!("/api/projects/{project}"),
            Some(&admin),
            Some(json!({ "prPreviews": { "enabled": true } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        r.json["prPreviews"],
        json!({ "enabled": true, "baseEnvironmentId": null })
    );

    let r = signed(&app, "pull_request", opened).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let envs = previews(&app).await;
    assert_eq!(envs.len(), 1, "{:?}", r.json);
    let (env, name, head) = envs[0].clone();
    assert_eq!((name.as_str(), head.as_deref()), ("pr-7", Some(SHA)));
    let r = app
        .call(
            Method::GET,
            &format!("/api/projects/{project}"),
            Some(&admin),
            None,
        )
        .await;
    let shown = r.json["environments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "pr-7")
        .unwrap()
        .clone();
    assert_eq!(
        (&shown["ephemeral"], &shown["prNumber"]),
        (&json!(true), &json!(7))
    );
    // The repository's services follow the pull request's branch.
    let branches: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT s.name, i.config->'source'->>'branch' FROM service_instances i
         JOIN services s ON s.id = i.service_id WHERE i.environment_id = $1 ORDER BY s.name",
    )
    .bind(env)
    .fetch_all(db)
    .await
    .unwrap();
    let feature = Some("feature/login".to_owned());
    assert_eq!(
        branches,
        [
            ("api".into(), feature.clone()),
            ("cache".into(), None),
            ("web".into(), feature)
        ]
    );
    // No sealed variables: the pull request's code never sees production's secrets.
    let vars: Vec<(String, bool)> = sqlx::query_as(
        "SELECT name, sealed FROM variables WHERE environment_id = $1 ORDER BY name",
    )
    .bind(env)
    .fetch_all(db)
    .await
    .unwrap();
    assert_eq!(vars, [("PLAIN".to_owned(), false)]);
    // A domain for the service production exposes.
    let hosts: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.name, d.hostname FROM domains d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id WHERE i.environment_id = $1",
    )
    .bind(env)
    .fetch_all(db)
    .await
    .unwrap();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].0, "web");
    assert!(hosts[0].1.starts_with("web-pr-7-"), "{}", hosts[0].1);
    // Everything starts in a new preview.
    let sha = Some(SHA.to_owned());
    assert_eq!(
        env_deployments(&app, env).await,
        [
            ("api".into(), sha.clone()),
            ("cache".into(), None),
            ("web".into(), sha.clone())
        ]
    );

    // One comment, with each service's URL and status.
    assert_eq!(
        run_jobs(&app, modula_server::github::pull_request::COMMENT_JOB).await,
        1
    );
    let comments = gh.comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 1);
    let (comment, number, body) = &comments[0];
    assert_eq!(*number, 7);
    assert!(body.starts_with(&format!("<!-- modula-preview:{project} -->")));
    assert!(
        body.contains(&format!("| web | https://{} | Building |", hosts[0].1)),
        "{body}"
    );
    assert!(body.contains("| cache | — | Deploying |"), "{body}");

    // A deployment failing updates the same comment.
    let (dep, node): (Id, Id) = sqlx::query_as(
        "SELECT d.id, d.node_id FROM deployments d JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id WHERE i.environment_id = $1 AND s.name = 'api'",
    )
    .bind(env)
    .fetch_one(db)
    .await
    .unwrap();
    let ev = modula_proto::node::v1::DeploymentEvent {
        deployment_id: dep.to_string(),
        phase: modula_proto::node::v1::DeploymentPhase::Failed as i32,
        message: "build failed".into(),
        ..Default::default()
    };
    modula_server::deploy::state::apply_event(db, node, &ev)
        .await
        .unwrap();
    // The stored id is used: nothing is listed to find it again.
    gh.comments.lock().unwrap()[0].2 = "edited on GitHub".into();
    assert_eq!(
        run_jobs(&app, modula_server::github::pull_request::COMMENT_JOB).await,
        1
    );
    let comments = gh.comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].0, *comment);
    assert!(
        comments[0].2.contains("| api | — | Failed |"),
        "{}",
        comments[0].2
    );

    // A push to the pull request redeploys the services it touches: web (no
    // watch paths), not api (only services/api/).
    *gh.files.lock().unwrap() = vec!["docs/README.md".into()];
    let sync = pull_request("synchronize", 7, SHA2, "MEMBER", own_head());
    let r = signed(&app, "pull_request", sync.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(previews(&app).await[0].2.as_deref(), Some(SHA2));
    let sha2 = Some(SHA2.to_owned());
    assert_eq!(
        env_deployments(&app, env).await,
        [
            ("api".into(), sha.clone()),
            ("cache".into(), None),
            ("web".into(), sha.clone()),
            ("web".into(), sha2.clone()),
        ]
    );
    // A push to the branch itself deploys nothing more (the preview follows
    // pull_request events), nor does a replayed event.
    let mut branch_push = push("refs/heads/feature/login", INSTALLATION, &["x"]);
    branch_push["after"] = json!(SHA2);
    signed(&app, "push", branch_push).await;
    signed(&app, "pull_request", sync).await;
    assert_eq!(env_deployments(&app, env).await.len(), 4);

    // Closing the pull request removes the preview and says so.
    let r = signed(
        &app,
        "pull_request",
        pull_request("closed", 7, SHA2, "MEMBER", own_head()),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(previews(&app).await.is_empty());
    run_jobs(&app, modula_server::github::pull_request::COMMENT_JOB).await;
    let comments = gh.comments.lock().unwrap().clone();
    assert_eq!(comments.len(), 1);
    assert!(comments[0].2.contains("removed"), "{}", comments[0].2);
    // Production was never touched.
    assert!(env_deployments(&app, production).await.is_empty());
}

/// Forks, authors without write access, and projects at their cap get no preview.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn untrusted_pull_requests_get_no_preview(pool: PgPool) {
    let (app, admin, gh) = app_with_gh(pool).await;
    let repo = json!({ "type": "repo", "repo": "acme/app", "branch": "main" });
    let ids = seed(&app, &admin, &[("api", json!({ "source": repo }))]).await;
    let (project, _) = project_of(&app, ids[0]).await;
    sqlx::query("UPDATE projects SET pr_previews = true WHERE id = $1")
        .bind(project)
        .execute(app.db())
        .await
        .unwrap();
    for body in [
        pull_request(
            "opened",
            8,
            SHA,
            "MEMBER",
            json!({ "full_name": "mallory/app" }),
        ),
        pull_request("opened", 8, SHA, "MEMBER", Value::Null),
        pull_request("opened", 8, SHA, "CONTRIBUTOR", own_head()),
        pull_request("synchronize", 8, SHA, "NONE", own_head()),
    ] {
        let r = signed(&app, "pull_request", body).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        assert!(r.json["ignored"].is_string(), "{:?}", r.json);
    }
    assert!(previews(&app).await.is_empty());
    assert!(deployments(&app).await.is_empty());

    // At the cap, a trusted pull request gets none either.
    for n in 0..modula_server::github::pull_request::MAX_PREVIEWS {
        sqlx::query(
            "INSERT INTO environments (id, project_id, name, ephemeral, pr_repo, pr_number)
             VALUES ($1, $2, $3, true, 'acme/app', $4)",
        )
        .bind(Id::new())
        .bind(project)
        .bind(format!("pr-{}", 100 + n))
        .bind(100 + n as i32)
        .execute(app.db())
        .await
        .unwrap();
    }
    let r = signed(
        &app,
        "pull_request",
        pull_request("opened", 9, SHA, "OWNER", own_head()),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(deployments(&app).await.is_empty());
    assert!(gh.comments.lock().unwrap().is_empty());
}

fn check_suite(sha: &str) -> Value {
    json!({
        "action": "completed",
        "check_suite": { "head_sha": sha, "status": "completed", "conclusion": "success", "app": { "id": 15368 } },
        "repository": { "full_name": "acme/App" },
        "installation": { "id": INSTALLATION },
    })
}

fn status_event(sha: &str, context: &str, state: &str) -> Value {
    json!({
        "sha": sha, "context": context, "state": state,
        "repository": { "full_name": "acme/App" },
        "installation": { "id": INSTALLATION },
    })
}

async fn deployment_status(app: &TestApp, id: &Value) -> (String, Option<String>) {
    sqlx::query_as("SELECT status, error FROM deployments WHERE id = $1")
        .bind(id.as_str().unwrap())
        .fetch_one(app.db())
        .await
        .unwrap()
}

/// With waitForCi a push waits at `waiting_ci` until the commit's other
/// checks pass (our own statuses and App don't count), fails when one
/// fails, and fails when CI takes too long.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn pushes_wait_for_ci(pool: PgPool) {
    let (app, admin, gh) = app_with_gh(pool).await;
    let repo = json!({ "type": "repo", "repo": "acme/app", "branch": "main" });
    seed(
        &app,
        &admin,
        &[("api", json!({ "source": repo, "waitForCi": true }))],
    )
    .await;
    let push_at = |sha: &str| {
        let mut b = push("refs/heads/main", INSTALLATION, &["src/a.rs"]);
        b["after"] = json!(sha);
        b
    };
    let r = signed(&app, "push", push_at(SHA)).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let first = r.json["deployments"][0].clone();
    assert_eq!(deployment_status(&app, &first).await.0, "waiting_ci");

    // Our own App's suite, a suite nothing ran in, and our own status don't
    // count; GitHub Actions is still running.
    let suite = |id: i64, app: i64, name: &str, status: &str, conclusion: Value, runs: i64| {
        json!({
            "id": id, "status": status, "conclusion": conclusion,
            "app": { "id": app, "name": name }, "latest_check_runs_count": runs,
        })
    };
    *gh.suites.lock().unwrap() = json!([
        suite(1, 1001, "Modula", "queued", Value::Null, 1),
        suite(2, 77, "Idle bot", "queued", Value::Null, 0),
        suite(3, 15368, "GitHub Actions", "in_progress", Value::Null, 2),
    ]);
    *gh.statuses.lock().unwrap() = json!([{ "context": "modula/api", "state": "pending" }]);
    let r = signed(&app, "status", status_event(SHA, "modula/api", "success")).await;
    assert_eq!(r.json["settled"], json!([]), "{:?}", r.json);
    let r = signed(&app, "check_suite", check_suite(SHA)).await;
    assert_eq!(r.json["settled"], json!([]), "{:?}", r.json);
    assert_eq!(deployment_status(&app, &first).await.0, "waiting_ci");

    // Actions passes: the build starts.
    gh.suites.lock().unwrap()[2] =
        suite(3, 15368, "GitHub Actions", "completed", json!("success"), 2);
    let r = signed(&app, "check_suite", check_suite(SHA)).await;
    assert_eq!(r.json["settled"], json!([first.clone()]), "{:?}", r.json);
    assert_eq!(deployment_status(&app, &first).await.0, "building");

    // A failing check fails the next push's deployment, and says so on the commit.
    let r = signed(&app, "push", push_at(SHA2)).await;
    let second = r.json["deployments"][0].clone();
    assert_eq!(deployment_status(&app, &second).await.0, "waiting_ci");
    *gh.suites.lock().unwrap() = json!([suite(
        3,
        15368,
        "GitHub Actions",
        "completed",
        json!("failure"),
        2
    )]);
    let r = signed(&app, "status", status_event(SHA2, "ci/legacy", "success")).await;
    assert_eq!(r.json["settled"], json!([second.clone()]), "{:?}", r.json);
    let (status, error) = deployment_status(&app, &second).await;
    assert_eq!(status, "failed");
    assert!(error.unwrap().contains("GitHub Actions concluded failure"));
    let reported: Vec<Value> =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'github.commit_status'")
            .fetch_all(app.db())
            .await
            .unwrap();
    assert!(reported.contains(&json!({ "deploymentId": second, "state": "failure" })));

    // CI that never finishes times out.
    *gh.suites.lock().unwrap() =
        json!([suite(3, 15368, "GitHub Actions", "queued", Value::Null, 2)]);
    let r = signed(&app, "push", push_at(SHA3)).await;
    let third = r.json["deployments"][0].clone();
    // One timeout per waiting deployment; only the third still waits.
    assert_eq!(
        run_jobs(&app, modula_server::github::ci::TIMEOUT_JOB).await,
        3
    );
    let (status, error) = deployment_status(&app, &third).await;
    assert_eq!(status, "failed");
    assert!(error.unwrap().contains("didn't finish"));
    // A late pass changes nothing.
    *gh.suites.lock().unwrap() = json!([suite(
        3,
        15368,
        "GitHub Actions",
        "completed",
        json!("success"),
        2
    )]);
    let r = signed(&app, "check_suite", check_suite(SHA3)).await;
    assert_eq!(r.json["settled"], json!([]), "{:?}", r.json);
    // The released first deployment was superseded by the next push, not
    // failed by its timeout.
    assert_eq!(deployment_status(&app, &first).await.0, "cancelled");
}

/// An App created before previews lacks their permissions and events: the
/// settings show what to approve, and where.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn old_apps_show_missing_permissions(pool: PgPool) {
    let (app, admin, _) = app_with_gh(pool).await;
    let r = app
        .call(Method::GET, "/api/github/apps", Some(&admin), None)
        .await;
    assert_eq!(r.json["apps"][0]["missingPermissions"], json!([]));
    modula_server::github::app::check_permissions(&app.state, 1001)
        .await
        .unwrap();
    let r = app
        .call(Method::GET, "/api/github/apps", Some(&admin), None)
        .await;
    let a = &r.json["apps"][0];
    assert_eq!(
        a["missingPermissions"],
        json!(["checks: read", "issues: write", "pull_requests: read"])
    );
    assert_eq!(
        a["missingEvents"],
        json!(["check_suite", "pull_request", "status"])
    );
    assert!(
        a["permissionsUrl"]
            .as_str()
            .unwrap()
            .ends_with("/settings/apps/modula-deploy-test/permissions"),
        "{a:?}"
    );
}
