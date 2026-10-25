//! `modula login` in a browser: the CLI starts a login and polls with its
//! device code; a signed-in user approves (sudo) or denies it by user code.

mod common;

use axum::http::{Method, Request, StatusCode};
use common::*;
use serde_json::{Value, json};
use sqlx::PgPool;

impl TestApp {
    /// A request the way the CLI sends it: no cookie, no Origin.
    async fn cli(&self, path: &str, body: Value) -> Resp {
        let b = Request::builder().method(Method::POST).uri(path);
        self.send(b, None, Some(body)).await
    }

    async fn start_login(&self) -> (String, String) {
        let r = self
            .cli("/api/cli/login", json!({ "hostname": "laptop.local" }))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let code = r.json["user_code"].as_str().unwrap().to_owned();
        assert_eq!(r.json["interval"], 2);
        assert_eq!(r.json["expires_in"], 600);
        assert_eq!(
            r.json["verification_url"].as_str().unwrap(),
            format!("{}/cli/login?code={code}", self.origin)
        );
        (r.json["device_code"].as_str().unwrap().to_owned(), code)
    }

    async fn poll(&self, device: &str) -> Resp {
        self.cli("/api/cli/login/token", json!({ "device_code": device }))
            .await
    }

    async fn review(&self, cookie: &str, action: &str, code: &str) -> Resp {
        self.call(
            Method::POST,
            &format!("/api/cli/login/{action}"),
            Some(cookie),
            Some(json!({ "userCode": code })),
        )
        .await
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn approving_mints_a_token_collected_once(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let (device, code) = app.start_login().await;
    let stored: Vec<u8> = sqlx::query_scalar("SELECT device_code_hash FROM cli_logins")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_ne!(stored, device.as_bytes(), "only the hash is stored");

    assert_eq!(app.poll(&device).await.json["status"], "pending");

    // The page shows which machine asked; the code is accepted in any case.
    let r = app
        .review(&admin, "lookup", &code.to_lowercase().replace('-', ""))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["hostname"], "laptop.local");
    assert_eq!(r.json["userCode"], code);

    let r = app.review(&admin, "approve", &code).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let r = app.poll(&device).await;
    assert_eq!(r.json["status"], "approved", "{:?}", r.json);
    assert_eq!(r.json["user"]["email"], "admin@example.com");
    let token = r.json["token"].as_str().unwrap();
    assert!(token.starts_with("mdt_"));

    // An ordinary token in the user's list, which works as a bearer.
    let list = app
        .call(Method::GET, "/api/account/tokens", Some(&admin), None)
        .await;
    assert_eq!(list.json[0]["name"], "CLI on laptop.local");
    let b = Request::builder()
        .uri("/api/auth/me")
        .header("authorization", format!("Bearer {token}"));
    assert_eq!(app.send(b, None, None).await.status, StatusCode::OK);

    // Single use: polling again yields nothing, and the code can't be reviewed again.
    let again = app.poll(&device).await;
    assert_eq!(again.json["status"], "expired");
    assert!(again.json.get("token").is_none());
    assert_eq!(
        app.review(&admin, "approve", &code).await.status,
        StatusCode::NOT_FOUND
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action IN ('account.cli_login_approved', 'account.token_created')",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(audited, 2);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn denied_logins_never_get_a_token(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let (device, code) = app.start_login().await;
    let r = app.review(&admin, "deny", &code).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = app.poll(&device).await;
    assert_eq!(r.json["status"], "denied");
    assert!(r.json.get("token").is_none());
    assert_eq!(
        app.review(&admin, "approve", &code).await.status,
        StatusCode::NOT_FOUND
    );
    let tokens: i64 = sqlx::query_scalar("SELECT count(*) FROM api_tokens")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(tokens, 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn expired_logins_cant_be_approved_or_collected(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let (device, code) = app.start_login().await;
    let (device2, code2) = app.start_login().await;
    assert_eq!(
        app.review(&admin, "approve", &code2).await.status,
        StatusCode::OK
    );
    sqlx::query("UPDATE cli_logins SET expires_at = now() - interval '1 minute'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(app.poll(&device).await.json["status"], "expired");
    assert_eq!(
        app.review(&admin, "lookup", &code).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        app.review(&admin, "approve", &code).await.status,
        StatusCode::NOT_FOUND
    );
    // Approved in time but collected too late: no token either.
    let r = app.poll(&device2).await;
    assert_eq!(r.json["status"], "expired");
    assert!(r.json.get("token").is_none());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn only_the_device_code_collects_the_token(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let (_, other_user) = app.user("eve@example.com").await;
    let (mine, code) = app.start_login().await;
    let (theirs, _) = app.start_login().await;
    assert_eq!(
        app.review(&admin, "approve", &code).await.status,
        StatusCode::OK
    );

    // The other machine's poll doesn't see this approval.
    assert_eq!(app.poll(&theirs).await.json["status"], "pending");
    // The user code (on screen, in the URL) is not a device code.
    for guess in [code.as_str(), &code.replace('-', ""), "mdc_nope"] {
        let r = app.poll(guess).await;
        assert_eq!(r.json["status"], "expired", "{guess}");
        assert!(r.json.get("token").is_none());
    }
    // Another signed-in user can't review an approved login...
    assert_eq!(
        app.review(&other_user, "deny", &code).await.status,
        StatusCode::NOT_FOUND
    );
    // ...and the token goes to whoever approved it.
    let r = app.poll(&mine).await;
    assert_eq!(r.json["user"]["email"], "admin@example.com");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn approving_needs_sudo_and_a_session(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let (device, code) = app.start_login().await;
    // Not signed in.
    let r = app
        .call(
            Method::POST,
            "/api/cli/login/approve",
            None,
            Some(json!({ "userCode": code })),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    // Signed in, but not recently.
    app.drop_sudo().await;
    assert_eq!(
        app.review(&admin, "approve", &code).await.code(),
        "sudo_required"
    );
    // Looking and denying don't need sudo.
    assert_eq!(
        app.review(&admin, "lookup", &code).await.status,
        StatusCode::OK
    );
    assert_eq!(app.poll(&device).await.json["status"], "pending");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn starting_and_polling_are_rate_limited(pool: PgPool) {
    let app = TestApp::new(pool);
    let mut limited = false;
    for _ in 0..25 {
        let r = app.cli("/api/cli/login", json!({})).await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            assert_eq!(r.code(), "rate_limited");
            limited = true;
            break;
        }
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    }
    assert!(limited, "starting logins is limited per IP");
    let mut limited = false;
    for _ in 0..130 {
        let r = app.poll("mdc_x").await;
        if r.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
    }
    assert!(limited, "polling is limited per IP");
}
