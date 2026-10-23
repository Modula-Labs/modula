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

/// GitHub's App endpoints: manifest conversion, installations, tokens, repos.
async fn mock_github() -> url::Url {
    use axum::extract::Path;
    use axum::routing::{get, post};
    let unauthorized = || (StatusCode::UNAUTHORIZED, axum::Json(json!({})));
    let app = axum::Router::new()
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
            get(move |h: axum::http::HeaderMap| async move {
                let auth = h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
                if auth != Some(&format!("Bearer {INSTALL_TOKEN}")) {
                    return unauthorized();
                }
                let repo = json!({ "full_name": "acme/secret", "private": true, "default_branch": "main" });
                (StatusCode::OK, axum::Json(json!({ "total_count": 1, "repositories": [repo] })))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}").parse().unwrap()
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
    assert_eq!(r.json["manifest"]["public"], false);
    r.json["state"].as_str().unwrap().to_owned()
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
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let url: url::Url = r.json["url"].as_str().unwrap().parse().unwrap();
    assert_eq!(url.path(), "/apps/modula-deploy-test/installations/new");
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
            None,
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
            &format!("/api/workspaces/{ws}/github/repos"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        r.json,
        json!([{ "fullName": "acme/secret", "private": true, "defaultBranch": "main" }])
    );
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
