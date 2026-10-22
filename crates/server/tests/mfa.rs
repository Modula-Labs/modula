mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_crypto::totp;
use serde_json::{Value, json};
use sqlx::PgPool;

fn now() -> u64 {
    time::OffsetDateTime::now_utc().unix_timestamp() as u64
}

/// Enroll TOTP for the admin; returns (raw secret, backup codes, session cookie).
async fn enroll(app: &TestApp) -> (Vec<u8>, Vec<String>, String) {
    let cookie = app.setup_admin().await;
    let r = app
        .call(Method::POST, "/api/account/totp", Some(&cookie), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let url = r.json["otpauthUrl"].as_str().unwrap();
    assert!(url.starts_with("otpauth://totp/Modula%20Cloud:admin%40example.com?secret="));
    let secret = data_encoding::BASE32_NOPAD
        .decode(r.json["secret"].as_str().unwrap().as_bytes())
        .unwrap();

    let r = app
        .call(
            Method::POST,
            "/api/account/totp/confirm",
            Some(&cookie),
            Some(json!({"code": "000000"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "wrong code is rejected");

    let code = totp::code_at(&secret, now()).unwrap();
    let r = app
        .call(
            Method::POST,
            "/api/account/totp/confirm",
            Some(&cookie),
            Some(json!({"code": code})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let codes: Vec<String> = serde_json::from_value(r.json["backupCodes"].clone()).unwrap();
    assert_eq!(codes.len(), 10);
    (secret, codes, cookie)
}

async fn password_step(app: &TestApp) -> String {
    let r = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": "admin@example.com", "password": PASSWORD})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["mfaRequired"], true);
    assert!(r.json.get("me").is_none());
    assert!(
        r.session_cookie().is_none(),
        "no full session before the second factor"
    );
    r.cookie(MFA_COOKIE).expect("pending cookie")
}

async fn second_step(app: &TestApp, pending: &str, body: Value) -> Resp {
    app.call_with_cookie_header(
        Method::POST,
        "/api/auth/mfa",
        &format!("{MFA_COOKIE}={pending}"),
        Some(body),
    )
    .await
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn totp_login_flow_with_replay_protection(pool: PgPool) {
    let app = TestApp::new(pool);
    let (secret, _, cookie) = enroll(&app).await;
    let r = app
        .call(Method::GET, "/api/account/security", Some(&cookie), None)
        .await;
    assert_eq!(r.json["totpEnabled"], true);
    assert_eq!(r.json["backupCodesRemaining"], 10);

    // The pending token is useless as a session cookie.
    let pending = password_step(&app).await;
    let r = app
        .call(Method::GET, "/api/auth/me", Some(&pending), None)
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // The confirm step consumed the current time step; a code from the next step works once.
    let code = totp::code_at(&secret, now() + 30).unwrap();
    let r = second_step(&app, &pending, json!({"totpCode": code})).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(r.cookie(MFA_COOKIE).is_none(), "pending cookie is cleared");
    let full = r.session_cookie().unwrap();
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&full), None)
            .await
            .status,
        StatusCode::OK
    );

    // The pending session is gone.
    let r = second_step(&app, &pending, json!({"totpCode": code})).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // Replaying the same code on a fresh login fails.
    let pending2 = password_step(&app).await;
    let r = second_step(&app, &pending2, json!({"totpCode": code})).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.code(), "invalid_credentials");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn backup_codes_are_single_use(pool: PgPool) {
    let app = TestApp::new(pool);
    let (_, codes, _) = enroll(&app).await;
    let pending = password_step(&app).await;
    let upper = codes[0].to_uppercase();
    let r = second_step(&app, &pending, json!({"backupCode": upper})).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let pending = password_step(&app).await;
    let r = second_step(&app, &pending, json!({"backupCode": codes[0]})).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "used code is rejected");
    let r = second_step(
        &app,
        &pending,
        json!({"backupCode": codes[1], "totpCode": "123456"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "exactly one factor");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn second_factor_guessing_burns_the_pending_session(pool: PgPool) {
    let app = TestApp::new(pool);
    let (secret, _, _) = enroll(&app).await;
    let pending = password_step(&app).await;
    for _ in 0..5 {
        let r = second_step(&app, &pending, json!({"totpCode": "000000"})).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    }
    let r = second_step(&app, &pending, json!({"totpCode": "000000"})).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    // Even the right code no longer works with this pending session.
    let good = totp::code_at(&secret, now() + 30).unwrap();
    let r = second_step(&app, &pending, json!({"totpCode": good})).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn sudo_uses_second_factor_and_rotates(pool: PgPool) {
    let app = TestApp::new(pool);
    let (secret, _, cookie) = enroll(&app).await;
    app.drop_sudo().await;
    let r = app
        .call(
            Method::POST,
            "/api/account/backup-codes",
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(r.code(), "sudo_required");

    // With 2FA on, a password alone is not enough.
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&cookie),
            Some(json!({"password": PASSWORD})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let code = totp::code_at(&secret, now() + 30).unwrap();
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&cookie),
            Some(json!({"totpCode": code})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let rotated = r.session_cookie().unwrap();
    assert_ne!(rotated, cookie);
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&cookie), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );

    let r = app
        .call(
            Method::POST,
            "/api/account/backup-codes",
            Some(&rotated),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["backupCodes"].as_array().unwrap().len(), 10);

    // Disabling 2FA returns login to a single step.
    assert_eq!(
        app.call(Method::DELETE, "/api/account/totp", Some(&rotated), None)
            .await
            .status,
        StatusCode::OK
    );
    let r = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": "admin@example.com", "password": PASSWORD})),
        )
        .await;
    assert_eq!(r.json["mfaRequired"], false);
    assert!(r.session_cookie().is_some());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn sudo_with_password_when_no_2fa(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    app.drop_sudo().await;
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&cookie),
            Some(json!({"password": "wrong password"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&cookie),
            Some(json!({"password": PASSWORD})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let rotated = r.session_cookie().unwrap();
    let r = app
        .call(Method::GET, "/api/account/security", Some(&rotated), None)
        .await;
    assert!(r.json["sudoUntil"].is_string());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn password_change_signs_out_other_sessions(pool: PgPool) {
    let app = TestApp::new(pool);
    let a = app.setup_admin().await;
    let b = app.login("admin@example.com").await;
    let r = app
        .call(
            Method::POST,
            "/api/account/password",
            Some(&a),
            Some(json!({"newPassword": "a brand new passphrase"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["sessionsRevoked"], 1);
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&a), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&b), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let r = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": "admin@example.com", "password": PASSWORD})),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "old password no longer works"
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn sessions_list_and_revoke(pool: PgPool) {
    let app = TestApp::new(pool);
    let a = app.setup_admin().await;
    let b = app.login("admin@example.com").await;
    let r = app
        .call(Method::GET, "/api/account/sessions", Some(&a), None)
        .await;
    let list = r.json.as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list.iter().filter(|s| s["current"] == true).count(), 1);
    let other = list.iter().find(|s| s["current"] == false).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/account/sessions/{other}"),
            Some(&a),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&b), None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn totp_secret_is_sealed_at_rest(pool: PgPool) {
    let app = TestApp::new(pool);
    let (secret, codes, _) = enroll(&app).await;
    let (sealed, pending): (Vec<u8>, Option<Vec<u8>>) =
        sqlx::query_as("SELECT totp_secret, totp_pending_secret FROM users")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(pending.is_none());
    assert!(!sealed.windows(secret.len()).any(|w| w == secret.as_slice()));
    let hashes: Vec<Vec<u8>> = sqlx::query_scalar("SELECT code_hash FROM backup_codes")
        .fetch_all(app.db())
        .await
        .unwrap();
    for c in &codes {
        let plain = c.replace('-', "");
        assert!(
            hashes
                .iter()
                .all(|h| !h.windows(plain.len()).any(|w| w == plain.as_bytes()))
        );
    }
}
