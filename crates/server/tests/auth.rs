mod common;

use axum::http::{Method, StatusCode, header};
use common::*;
use secrecy::ExposeSecret;
use serde_json::json;
use sqlx::PgPool;

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn setup_flow_is_one_time_and_token_gated(pool: PgPool) {
    let app = TestApp::new(pool);
    let r = app.call(Method::GET, "/api/instance", None, None).await;
    assert_eq!(r.json["setupRequired"], true);

    let token = modula_server::auth::setup::issue_if_pending(app.db())
        .await
        .unwrap()
        .unwrap();
    let body = |t: &str| json!({"setupToken": t, "email": "Admin@Example.com", "name": "Admin", "password": PASSWORD});

    // Wrong and malformed tokens are rejected.
    let wrong = modula_crypto::token::generate(modula_crypto::token::prefix::SETUP).unwrap();
    let r = app
        .call(
            Method::POST,
            "/api/auth/setup",
            None,
            Some(body(wrong.plaintext.expose_secret())),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = app
        .call(Method::POST, "/api/auth/setup", None, Some(body("nope")))
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Correct token creates the admin, a workspace, and a session.
    let r = app
        .call(
            Method::POST,
            "/api/auth/setup",
            None,
            Some(body(token.expose_secret())),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["user"]["email"], "admin@example.com");
    assert_eq!(r.json["user"]["isInstanceAdmin"], true);
    assert_eq!(r.json["workspaces"][0]["role"], "admin");
    let cookie = r.session_cookie().unwrap();
    let raw = r.set_cookie_header().unwrap();
    for attr in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
        assert!(raw.contains(attr), "cookie missing {attr}: {raw}");
    }

    let r = app
        .call(Method::GET, "/api/auth/me", Some(&cookie), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let r = app.call(Method::GET, "/api/instance", None, None).await;
    assert_eq!(r.json["setupRequired"], false);

    // Token is consumed; setup cannot run again, and no new tokens are minted.
    let r = app
        .call(
            Method::POST,
            "/api/auth/setup",
            None,
            Some(body(token.expose_secret())),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert!(
        modula_server::auth::setup::issue_if_pending(app.db())
            .await
            .unwrap()
            .is_none()
    );

    let failed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'instance.setup_failed'")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(failed, 2);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn expired_or_replaced_setup_token_is_rejected(pool: PgPool) {
    let app = TestApp::new(pool);
    let old = modula_server::auth::setup::issue_if_pending(app.db())
        .await
        .unwrap()
        .unwrap();
    let new = modula_server::auth::setup::issue_if_pending(app.db())
        .await
        .unwrap()
        .unwrap();
    let body =
        |t: &str| json!({"setupToken": t, "email": "a@b.co", "name": "A", "password": PASSWORD});

    let r = app
        .call(
            Method::POST,
            "/api/auth/setup",
            None,
            Some(body(old.expose_secret())),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "a restart replaces the token"
    );

    sqlx::query("UPDATE instance SET setup_token_expires_at = now() - interval '1 second'")
        .execute(app.db())
        .await
        .unwrap();
    let r = app
        .call(
            Method::POST,
            "/api/auth/setup",
            None,
            Some(body(new.expose_secret())),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn login_failures_are_uniform_and_audited(pool: PgPool) {
    let app = TestApp::new(pool);
    app.setup_admin().await;

    let wrong_pw = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": "admin@example.com", "password": "wrong password!"})),
        )
        .await;
    let no_user = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": "ghost@example.com", "password": PASSWORD})),
        )
        .await;
    for r in [&wrong_pw, &no_user] {
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
        assert_eq!(r.code(), "invalid_credentials");
        assert!(r.session_cookie().is_none());
    }
    assert_eq!(
        wrong_pw.json, no_user.json,
        "no user enumeration via response body"
    );

    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'auth.login_failed'")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(n, 2);

    // Email is case-insensitive.
    let r = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": " ADMIN@example.com ", "password": PASSWORD})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn login_rotates_and_logout_revokes(pool: PgPool) {
    let app = TestApp::new(pool);
    let first = app.setup_admin().await;

    // Logging in while holding a session replaces it (no fixation).
    let r = app
        .send(
            axum::http::Request::builder()
                .method(Method::POST)
                .uri("/api/auth/login")
                .header(header::ORIGIN, ORIGIN),
            Some(&first),
            Some(json!({"email": "admin@example.com", "password": PASSWORD})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let second = r.session_cookie().unwrap();
    assert_ne!(first, second);
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&first), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&second), None)
            .await
            .status,
        StatusCode::OK
    );

    let r = app
        .call(Method::POST, "/api/auth/logout", Some(&second), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.set_cookie_header().unwrap().contains("Max-Age=0"));
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&second), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn idle_and_expired_sessions_are_rejected(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    sqlx::query("UPDATE sessions SET last_seen_at = now() - interval '8 days'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&cookie), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let cookie = app.login("admin@example.com").await;
    sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&cookie), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        modula_server::auth::session::delete_expired(app.db())
            .await
            .unwrap(),
        2
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn login_is_rate_limited(pool: PgPool) {
    let app = TestApp::new(pool);
    app.setup_admin().await;
    let mut last = None;
    for _ in 0..11 {
        last = Some(
            app.call(
                Method::POST,
                "/api/auth/login",
                None,
                Some(json!({"email": "admin@example.com", "password": "bad password 1"})),
            )
            .await,
        );
    }
    let r = last.unwrap();
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(r.headers.get(header::RETRY_AFTER).is_some());
    // Even the right password is refused while limited.
    let r = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": "admin@example.com", "password": PASSWORD})),
        )
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn mutations_require_exact_origin(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    let req = |origin: Option<&str>| {
        let b = axum::http::Request::builder()
            .method(Method::POST)
            .uri("/api/workspaces");
        match origin {
            Some(o) => b.header(header::ORIGIN, o),
            None => b,
        }
    };
    for origin in [
        None,
        Some("https://evil.test"),
        Some("https://cloud.test.evil.test"),
        Some("http://cloud.test"),
    ] {
        let r = app
            .send(req(origin), Some(&cookie), Some(json!({"name": "x"})))
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "origin {origin:?}");
    }
    let r = app
        .send(req(Some(ORIGIN)), Some(&cookie), Some(json!({"name": "x"})))
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn security_headers_on_every_response(pool: PgPool) {
    let app = TestApp::new(pool);
    for path in ["/api/health", "/api/nope", "/api/auth/me"] {
        let r = app.call(Method::GET, path, None, None).await;
        for h in [
            "content-security-policy",
            "x-content-type-options",
            "x-frame-options",
            "strict-transport-security",
            "referrer-policy",
        ] {
            assert!(r.headers.contains_key(h), "{path} missing {h}");
        }
        assert_eq!(r.headers["cache-control"], "no-store");
        assert!(
            r.headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn unknown_fields_and_bad_input_are_rejected(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    let r = app
        .call(
            Method::POST,
            "/api/workspaces",
            Some(&cookie),
            Some(json!({"name": "ok", "owner": "someone-else"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    let r = app
        .call(
            Method::POST,
            "/api/workspaces",
            Some(&cookie),
            Some(json!({"name": "  "})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
