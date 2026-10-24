//! GitHub App: the manifest flow's single-use state, webhook authentication
//! and replay protection, and push-to-deploy matching, against a mock GitHub.

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

/// GitHub's App endpoints: manifest conversion, installations, tokens, repos,
/// branches. The counter tracks calls to the listing endpoints.
async fn mock_github_counted() -> (url::Url, Counter) {
    use axum::extract::{Path, Query};
    use axum::routing::{get, post};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    let calls = Counter::default();
    let (c1, c2) = (calls.clone(), calls.clone());
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
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}").parse().unwrap(), calls)
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
    // Installable on any account or org (binding still needs a flow started here).
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
        sqlx::query_as("SELECT slug, webhook_secret_sealed FROM github_app")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(slug, "modula-deploy-test");
    assert!(
        !sealed
            .windows(WEBHOOK_SECRET.len())
            .any(|w| w == WEBHOOK_SECRET.as_bytes())
    );
    // One App per instance.
    let r = app
        .call(
            Method::POST,
            "/api/github/app/manifest",
            Some(&admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
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
        json!({ "contents": "read", "metadata": "read", "statuses": "write" })
    );
    assert_eq!(m["default_events"], json!(["push"]));
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
        &format!("installation_id={INSTALLATION}&setup_action=install&state={st}"),
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
    let apps: i64 = sqlx::query_scalar("SELECT count(*) FROM github_app")
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
        &format!("installation_id={INSTALLATION}&setup_action=install&state={st}"),
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
        "INSERT INTO github_installations (id, workspace_id, account_login, account_type)
         VALUES ($1, $2, 'acme', 'Organization')",
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
        let r = signed(&app, "push", body).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
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
        format!("/api/github/install/callback?installation_id={id}&setup_action=install&state={st}")
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
        .call(Method::GET, "/api/github/app", Some(&admin), None)
        .await;
    assert_eq!(r.json["webhookUrl"], format!("{ORIGIN}/api/github/webhook"));
    assert_eq!(r.json["ownerLogin"], "acme");
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
