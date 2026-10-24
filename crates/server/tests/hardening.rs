//! Abuse resistance: login lockout that strangers can't trigger for the
//! owner, password changes that revoke API tokens, database writes
//! behind Admin/sudo, the workspace upload quota, and the
//! webhook test rate limit.

mod common;

use axum::http::{Method, Request, StatusCode, header};
use common::*;
use modula_id::Id;
use serde_json::{Value, json};
use sqlx::PgPool;

/// An app that trusts the test peer as a proxy, so `X-Forwarded-For` sets
/// the client IP.
fn behind_proxy(pool: PgPool) -> TestApp {
    let mut cfg = config();
    cfg.trusted_proxies = vec!["10.0.0.1".parse().unwrap()];
    TestApp::with_config(pool, cfg)
}

async fn login_from(app: &TestApp, ip: &str, password: &str) -> Resp {
    let b = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::ORIGIN, ORIGIN)
        .header("x-forwarded-for", ip);
    app.send(
        b,
        None,
        Some(json!({ "email": "admin@example.com", "password": password })),
    )
    .await
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn failed_logins_elsewhere_dont_lock_the_owner_out(pool: PgPool) {
    let app = behind_proxy(pool);
    app.setup_admin().await;
    for _ in 0..10 {
        let r = login_from(&app, "203.0.113.7", "wrong password 1").await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{:?}", r.json);
    }
    // The guesser is now refused for this email, even with the right password...
    let r = login_from(&app, "203.0.113.7", PASSWORD).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{:?}", r.json);
    assert!(r.headers.get(header::RETRY_AFTER).is_some());
    // ...but the owner, from their own address, signs in.
    let r = login_from(&app, "198.51.100.20", PASSWORD).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // Successes never count toward the lockout.
    for _ in 0..12 {
        let r = login_from(&app, "198.51.100.20", PASSWORD).await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    }
}

async fn new_token(app: &TestApp, cookie: &str) -> String {
    let r = app
        .call(
            Method::POST,
            "/api/account/tokens",
            Some(cookie),
            Some(json!({ "name": "laptop" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    r.json["token"].as_str().unwrap().to_owned()
}

async fn with_token(app: &TestApp, token: &str) -> StatusCode {
    let b = Request::builder()
        .uri("/api/auth/me")
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    app.send(b, None, None).await.status
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn password_change_revokes_sessions_and_tokens(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let other = app.login("admin@example.com").await;
    let token = new_token(&app, &admin).await;
    assert_eq!(with_token(&app, &token).await, StatusCode::OK);

    // Keeping tokens is an explicit choice.
    let r = app
        .call(
            Method::POST,
            "/api/account/password",
            Some(&admin),
            Some(json!({ "newPassword": "another long password", "keepTokens": true })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["sessionsRevoked"], 1);
    assert_eq!(r.json["tokensRevoked"], 0);
    assert_eq!(with_token(&app, &token).await, StatusCode::OK);
    let r = app
        .call(Method::GET, "/api/auth/me", Some(&other), None)
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // By default every token goes too; this session stays.
    let r = app
        .call(
            Method::POST,
            "/api/account/password",
            Some(&admin),
            Some(json!({ "newPassword": "a third long password" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["tokensRevoked"], 1);
    assert_eq!(with_token(&app, &token).await, StatusCode::UNAUTHORIZED);
    let r = app
        .call(Method::GET, "/api/auth/me", Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

/// A workspace with one Postgres-looking service instance; `(app, admin
/// cookie, workspace, instance)`.
async fn database_fixture(pool: PgPool) -> (TestApp, String, Id, String) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let p = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(&admin),
            Some(json!({ "name": "shop" })),
        )
        .await;
    let project = p.json["id"].as_str().unwrap().to_owned();
    let s = app
        .call(
            Method::POST,
            &format!("/api/projects/{project}/services"),
            Some(&admin),
            Some(json!({ "name": "db", "config": { "source": { "type": "image", "image": "postgres:17" } } })),
        )
        .await;
    assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
    let instance = s.json["instances"][0]["id"].as_str().unwrap().to_owned();
    (app, admin, ws, instance)
}

fn error_code(r: &Resp) -> &str {
    r.json["error"]["code"].as_str().unwrap_or_default()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn database_writes_need_admin_and_sudo(pool: PgPool) {
    let (app, admin, ws, instance) = database_fixture(pool).await;
    let (member_id, member) = app.user("member@example.com").await;
    app.add_member(ws, member_id, "member").await;
    let base = format!("/api/service-instances/{instance}/database");
    let writes: Vec<(Method, String, Value)> = vec![
        (
            Method::POST,
            format!("{base}/rows"),
            json!({ "table": "t", "values": {} }),
        ),
        (Method::POST, format!("{base}/vacuum"), json!({})),
        (
            Method::POST,
            format!("{base}/extensions"),
            json!({ "name": "pg_trgm", "action": "install" }),
        ),
        (
            Method::POST,
            format!("{base}/query"),
            json!({ "sql": "DELETE FROM t" }),
        ),
    ];
    // Members (even fresh from sign-in, with sudo) can't change data.
    for (m, path, body) in &writes {
        let r = app
            .call(m.clone(), path, Some(&member), Some(body.clone()))
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}: {:?}", r.json);
        assert_ne!(error_code(&r), "sudo_required", "{path}");
    }
    // Admins need a recent re-authentication.
    app.drop_sudo().await;
    for (m, path, body) in &writes {
        let r = app
            .call(m.clone(), path, Some(&admin), Some(body.clone()))
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}: {:?}", r.json);
        assert_eq!(error_code(&r), "sudo_required", "{path}");
    }
    // Credentials: Members may, without sudo (API tokens, which never have
    // it, need them for `modula connect`); each read is audited.
    let r = app
        .call(
            Method::GET,
            &format!("{base}/credentials"),
            Some(&member),
            None,
        )
        .await;
    assert_ne!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    assert_ne!(error_code(&r), "sudo_required", "{:?}", r.json);
    // A read-only query is still just DatabaseRead (it fails later: no node).
    let r = app
        .call(
            Method::POST,
            &format!("{base}/query"),
            Some(&member),
            Some(json!({ "sql": "SELECT 1", "readOnly": true })),
        )
        .await;
    assert_ne!(error_code(&r), "sudo_required", "{:?}", r.json);
    assert_ne!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn workspace_upload_quota(pool: PgPool) {
    let (app, admin, ws, instance) = database_fixture(pool).await;
    // Earlier today the workspace uploaded all but a few bytes of its quota.
    let used = modula_server::api::uploads::WORKSPACE_DAILY_BYTES as i64 - 10;
    sqlx::query(
        "INSERT INTO source_uploads (id, workspace_id, service_instance_id, sha256, size_bytes)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(Id::new())
    .bind(ws)
    .bind(instance.parse::<Id>().unwrap())
    .bind("0".repeat(64))
    .bind(used)
    .execute(app.db())
    .await
    .unwrap();
    let body = {
        use std::io::Write;
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&[7u8; 4096]).unwrap();
        gz.finish().unwrap()
    };
    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/service-instances/{instance}/uploads"))
        .header(header::ORIGIN, ORIGIN)
        .header(header::COOKIE, format!("{COOKIE}={admin}"))
        .header(header::CONTENT_TYPE, "application/gzip")
        .body(axum::body::Body::from(body))
        .unwrap();
    let res = tower::ServiceExt::oneshot(app.router.clone(), req)
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // A day later the room is back.
    sqlx::query("UPDATE source_uploads SET created_at = now() - interval '25 hours'")
        .execute(app.db())
        .await
        .unwrap();
    let left: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM source_uploads WHERE created_at > now() - interval '1 day'",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(left, 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn webhook_tests_are_rate_limited(pool: PgPool) {
    let (app, admin, _, _) = database_fixture(pool).await;
    let project: Id = sqlx::query_scalar("SELECT id FROM projects")
        .fetch_one(app.db())
        .await
        .unwrap();
    let r = app
        .call(
            Method::POST,
            &format!("/api/projects/{project}/webhooks"),
            Some(&admin),
            Some(json!({ "url": "https://hooks.invalid/x", "events": ["deployment.failed"] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let id = r.json["id"].as_str().unwrap().to_owned();
    let mut last = StatusCode::OK;
    for _ in 0..11 {
        last = app
            .call(
                Method::POST,
                &format!("/api/webhooks/{id}/test"),
                Some(&admin),
                None,
            )
            .await
            .status;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

/// `peek`/`hit` against a real Redis when MODULA_TEST_REDIS_URL is set.
#[tokio::test]
async fn redis_peek_and_hit() {
    let Ok(url) = std::env::var("MODULA_TEST_REDIS_URL") else {
        eprintln!("skipping: MODULA_TEST_REDIS_URL not set");
        return;
    };
    use modula_server::ratelimit::{Limit, RateLimiter};
    let client = redis::Client::open(url).unwrap();
    let rl = RateLimiter::Redis(redis::aio::ConnectionManager::new(client).await.unwrap());
    let key = Id::new().to_string();
    let lim = Limit::per(2, std::time::Duration::from_millis(400));
    // Peeking never counts.
    for _ in 0..5 {
        rl.peek("test-fail", &key, lim).await.unwrap();
    }
    rl.hit("test-fail", &key, lim).await.unwrap();
    rl.peek("test-fail", &key, lim).await.unwrap();
    rl.hit("test-fail", &key, lim).await.unwrap();
    assert!(rl.peek("test-fail", &key, lim).await.is_err());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    rl.peek("test-fail", &key, lim).await.unwrap();
}
