//! Passkeys as a second factor, driven by a software authenticator (ES256,
//! "none" attestation) that answers the server's real challenges.

mod common;

use axum::http::{Method, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use common::*;
use modula_crypto::totp;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// Just enough CBOR for an attestation object and a COSE key.
enum Cbor {
    Int(i64),
    Bytes(Vec<u8>),
    Text(&'static str),
    Map(Vec<(Cbor, Cbor)>),
}

impl Cbor {
    fn encode(&self, out: &mut Vec<u8>) {
        fn head(out: &mut Vec<u8>, major: u8, n: u64) {
            let m = major << 5;
            match n {
                0..=23 => out.push(m | n as u8),
                24..=0xff => out.extend([m | 24, n as u8]),
                0x100..=0xffff => {
                    out.push(m | 25);
                    out.extend((n as u16).to_be_bytes());
                }
                _ => {
                    out.push(m | 26);
                    out.extend((n as u32).to_be_bytes());
                }
            }
        }
        match self {
            Self::Int(i) if *i >= 0 => head(out, 0, *i as u64),
            Self::Int(i) => head(out, 1, (-1 - *i) as u64),
            Self::Bytes(b) => {
                head(out, 2, b.len() as u64);
                out.extend(b);
            }
            Self::Text(t) => {
                head(out, 3, t.len() as u64);
                out.extend(t.as_bytes());
            }
            Self::Map(kv) => {
                head(out, 5, kv.len() as u64);
                for (k, v) in kv {
                    k.encode(out);
                    v.encode(out);
                }
            }
        }
    }
}

struct Authenticator {
    key: EcdsaKeyPair,
    cred_id: Vec<u8>,
    rp_id: String,
    origin: String,
    counter: u32,
}

impl Authenticator {
    fn new(app: &TestApp) -> Self {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let mut cred_id = vec![0u8; 16];
        ring::rand::SecureRandom::fill(&rng, &mut cred_id).unwrap();
        Self {
            key,
            cred_id,
            rp_id: "cloud.test".into(),
            origin: app.origin.clone(),
            counter: 0,
        }
    }

    fn client_data(&self, kind: &str, options: &Value) -> Vec<u8> {
        let challenge = options["publicKey"]["challenge"].as_str().unwrap();
        serde_json::to_vec(&json!({
            "type": kind,
            "challenge": challenge,
            "origin": self.origin,
            "crossOrigin": false,
        }))
        .unwrap()
    }

    /// Flags: user present + user verified (+ attested credential data).
    fn auth_data(&self, flags: u8, attested: Option<Vec<u8>>) -> Vec<u8> {
        let mut d = Sha256::digest(self.rp_id.as_bytes()).to_vec();
        d.push(flags);
        d.extend(self.counter.to_be_bytes());
        if let Some(a) = attested {
            d.extend(a);
        }
        d
    }

    /// Answer `navigator.credentials.create` options (as the browser's toJSON()).
    fn create(&self, options: &Value) -> Value {
        let pk = self.key.public_key().as_ref();
        let cose = Cbor::Map(vec![
            (Cbor::Int(1), Cbor::Int(2)),
            (Cbor::Int(3), Cbor::Int(-7)),
            (Cbor::Int(-1), Cbor::Int(1)),
            (Cbor::Int(-2), Cbor::Bytes(pk[1..33].to_vec())),
            (Cbor::Int(-3), Cbor::Bytes(pk[33..65].to_vec())),
        ]);
        let mut attested = vec![0u8; 16]; // AAGUID
        attested.extend((self.cred_id.len() as u16).to_be_bytes());
        attested.extend(&self.cred_id);
        cose.encode(&mut attested);
        let auth_data = self.auth_data(0x45, Some(attested));
        let mut att = Vec::new();
        Cbor::Map(vec![
            (Cbor::Text("fmt"), Cbor::Text("none")),
            (Cbor::Text("attStmt"), Cbor::Map(vec![])),
            (Cbor::Text("authData"), Cbor::Bytes(auth_data)),
        ])
        .encode(&mut att);
        let id = B64.encode(&self.cred_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "clientDataJSON": B64.encode(self.client_data("webauthn.create", options)),
                "attestationObject": B64.encode(att),
            },
            "clientExtensionResults": {},
        })
    }

    /// Answer `navigator.credentials.get` options.
    fn get(&mut self, options: &Value) -> Value {
        self.counter += 1;
        let auth_data = self.auth_data(0x05, None);
        let client_data = self.client_data("webauthn.get", options);
        let mut signed = auth_data.clone();
        signed.extend(Sha256::digest(&client_data));
        let sig = self.key.sign(&SystemRandom::new(), &signed).unwrap();
        let id = B64.encode(&self.cred_id);
        json!({
            "id": id,
            "rawId": id,
            "type": "public-key",
            "response": {
                "authenticatorData": B64.encode(auth_data),
                "clientDataJSON": B64.encode(client_data),
                "signature": B64.encode(sig.as_ref()),
            },
            "clientExtensionResults": {},
        })
    }
}

/// Register a passkey for the signed-in user; returns the confirm response.
async fn add_passkey(app: &TestApp, cookie: &str, auth: &Authenticator) -> Value {
    let r = app
        .call(Method::POST, "/api/account/passkeys", Some(cookie), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["publicKey"]["rp"]["id"], "cloud.test");
    let r = app
        .call(
            Method::POST,
            "/api/account/passkeys/confirm",
            Some(cookie),
            Some(json!({"name": "Laptop", "credential": auth.create(&r.json)})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    r.json
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
    r.cookie(MFA_COOKIE).expect("pending cookie")
}

async fn pending_call(app: &TestApp, pending: &str, path: &str, body: Option<Value>) -> Resp {
    app.call_with_cookie_header(Method::POST, path, &format!("{MFA_COOKIE}={pending}"), body)
        .await
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn passkey_is_a_second_factor_at_login(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    let mut auth = Authenticator::new(&app);
    let added = add_passkey(&app, &cookie, &auth).await;
    assert_eq!(
        added["backupCodes"].as_array().unwrap().len(),
        10,
        "the first second factor comes with backup codes"
    );

    // The same authenticator can't be added twice.
    let r = app
        .call(Method::POST, "/api/account/passkeys", Some(&cookie), None)
        .await;
    let excluded = r.json["publicKey"]["excludeCredentials"]
        .as_array()
        .unwrap();
    assert_eq!(excluded.len(), 1);
    let r = app
        .call(
            Method::POST,
            "/api/account/passkeys/confirm",
            Some(&cookie),
            Some(json!({"name": "Again", "credential": auth.create(&r.json)})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM passkeys")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(n, 1);

    let sec = app
        .call(Method::GET, "/api/account/security", Some(&cookie), None)
        .await
        .json;
    assert_eq!(sec["passkeyRpId"], "cloud.test");
    assert_eq!(sec["passkeys"][0]["name"], "Laptop");
    assert_eq!(sec["totpEnabled"], false);

    // Password alone now only gets a pending session.
    let pending = password_step(&app).await;
    let r = app
        .call_with_cookie_header(
            Method::GET,
            "/api/auth/mfa",
            &format!("{MFA_COOKIE}={pending}"),
            None,
        )
        .await;
    assert_eq!(r.json, json!({"totp": false, "passkey": true}));
    let opts = pending_call(&app, &pending, "/api/auth/mfa/passkey", None).await;
    assert_eq!(opts.status, StatusCode::OK, "{:?}", opts.json);
    let assertion = auth.get(&opts.json);
    let r = pending_call(
        &app,
        &pending,
        "/api/auth/mfa",
        Some(json!({"passkey": assertion})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(r.session_cookie().is_some());
    let method: String = sqlx::query_scalar(
        "SELECT metadata->>'method' FROM audit_log WHERE action = 'auth.login' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(method, "passkey");

    // A replayed assertion fails: its challenge was consumed with the session.
    let pending = password_step(&app).await;
    pending_call(&app, &pending, "/api/auth/mfa/passkey", None).await;
    let r = pending_call(
        &app,
        &pending,
        "/api/auth/mfa",
        Some(json!({"passkey": assertion})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn passkey_unlocks_sudo(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    let mut auth = Authenticator::new(&app);
    add_passkey(&app, &cookie, &auth).await;
    app.drop_sudo().await;

    // With a passkey enrolled, the password is no longer enough.
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&cookie),
            Some(json!({"password": PASSWORD})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    let opts = app
        .call(Method::POST, "/api/auth/sudo/passkey", Some(&cookie), None)
        .await;
    assert_eq!(opts.status, StatusCode::OK, "{:?}", opts.json);
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&cookie),
            Some(json!({"passkey": auth.get(&opts.json)})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_ne!(r.session_cookie().unwrap(), cookie, "session rotated");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn wrong_key_and_cloned_counter_are_rejected(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    let mut auth = Authenticator::new(&app);
    add_passkey(&app, &cookie, &auth).await;

    // Another key claiming the same credential id.
    let pending = password_step(&app).await;
    let opts = pending_call(&app, &pending, "/api/auth/mfa/passkey", None).await;
    let mut impostor = Authenticator::new(&app);
    impostor.cred_id = auth.cred_id.clone();
    let r = pending_call(
        &app,
        &pending,
        "/api/auth/mfa",
        Some(json!({"passkey": impostor.get(&opts.json)})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);

    // An assertion for another origin.
    let opts = pending_call(&app, &pending, "/api/auth/mfa/passkey", None).await;
    let real_origin = std::mem::replace(&mut auth.origin, "https://evil.test".into());
    let r = pending_call(
        &app,
        &pending,
        "/api/auth/mfa",
        Some(json!({"passkey": auth.get(&opts.json)})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    auth.origin = real_origin;

    // Sign in once (counter moves forward), then a counter that went back.
    let opts = pending_call(&app, &pending, "/api/auth/mfa/passkey", None).await;
    let r = pending_call(
        &app,
        &pending,
        "/api/auth/mfa",
        Some(json!({"passkey": auth.get(&opts.json)})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let pending = password_step(&app).await;
    let opts = pending_call(&app, &pending, "/api/auth/mfa/passkey", None).await;
    auth.counter = 0;
    let r = pending_call(
        &app,
        &pending,
        "/api/auth/mfa",
        Some(json!({"passkey": auth.get(&opts.json)})),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED, "cloned authenticator");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn passkeys_for_another_domain_cannot_sign_in(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    let auth = Authenticator::new(&app);
    let codes = add_passkey(&app, &cookie, &auth).await["backupCodes"].clone();
    // As if the dashboard's domain changed since it was registered.
    sqlx::query("UPDATE passkeys SET rp_id = 'old.test'")
        .execute(app.db())
        .await
        .unwrap();

    // Still a second factor (no silent downgrade); recovery codes get in.
    let pending = password_step(&app).await;
    let r = pending_call(&app, &pending, "/api/auth/mfa/passkey", None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = pending_call(
        &app,
        &pending,
        "/api/auth/mfa",
        Some(json!({"backupCode": codes[0]})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn backup_codes_live_while_any_second_factor_does(pool: PgPool) {
    let app = TestApp::new(pool);
    let cookie = app.setup_admin().await;
    let auth = Authenticator::new(&app);
    let codes = add_passkey(&app, &cookie, &auth).await["backupCodes"].clone();

    // Adding TOTP next keeps the saved codes.
    let r = app
        .call(Method::POST, "/api/account/totp", Some(&cookie), None)
        .await;
    let secret = data_encoding::BASE32_NOPAD
        .decode(r.json["secret"].as_str().unwrap().as_bytes())
        .unwrap();
    let now = time::OffsetDateTime::now_utc().unix_timestamp() as u64;
    let r = app
        .call(
            Method::POST,
            "/api/account/totp/confirm",
            Some(&cookie),
            Some(json!({"code": totp::code_at(&secret, now).unwrap()})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["backupCodes"], json!([]));

    // Turning TOTP off leaves the passkey, so the codes stay.
    let r = app
        .call(Method::DELETE, "/api/account/totp", Some(&cookie), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let sec = app
        .call(Method::GET, "/api/account/security", Some(&cookie), None)
        .await
        .json;
    assert_eq!(sec["backupCodesRemaining"], 10);

    // Removing the last passkey needs sudo, then drops the codes and the 2FA step.
    let id = sec["passkeys"][0]["id"].as_str().unwrap().to_owned();
    let r = app
        .call(
            Method::PATCH,
            &format!("/api/account/passkeys/{id}"),
            Some(&cookie),
            Some(json!({"name": "Work laptop"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    app.drop_sudo().await;
    let path = format!("/api/account/passkeys/{id}");
    let r = app.call(Method::DELETE, &path, Some(&cookie), None).await;
    assert_eq!(r.code(), "sudo_required");
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&cookie),
            Some(json!({"backupCode": codes[0]})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let cookie = r.session_cookie().unwrap();
    let r = app.call(Method::DELETE, &path, Some(&cookie), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let sec = app
        .call(Method::GET, "/api/account/security", Some(&cookie), None)
        .await
        .json;
    assert_eq!(sec["backupCodesRemaining"], 0);
    let r = app
        .call(
            Method::POST,
            "/api/auth/login",
            None,
            Some(json!({"email": "admin@example.com", "password": PASSWORD})),
        )
        .await;
    assert_eq!(r.json["mfaRequired"], false);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn passkeys_need_a_dashboard_domain(pool: PgPool) {
    let mut cfg = config();
    cfg.public_url = "https://203.0.113.7".parse().unwrap();
    let app = TestApp::with_config(pool, cfg);
    let cookie = app.setup_admin().await;
    let sec = app
        .call(Method::GET, "/api/account/security", Some(&cookie), None)
        .await
        .json;
    assert_eq!(sec["passkeyRpId"], Value::Null);
    let r = app
        .call(Method::POST, "/api/account/passkeys", Some(&cookie), None)
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
