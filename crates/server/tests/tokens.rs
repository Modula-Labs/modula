//! API tokens: creation (sudo only), bearer authentication on ordinary routes,
//! workspace restriction, revocation/expiry, and the CSRF exemption that only
//! a valid token earns.

mod common;

use axum::http::{Method, Request, StatusCode, header};
use common::*;
use modula_id::Id;
use serde_json::{Value, json};
use sqlx::PgPool;

impl TestApp {
    async fn create_token(&self, cookie: &str, body: Value) -> Resp {
        self.call(
            Method::POST,
            "/api/account/tokens",
            Some(cookie),
            Some(body),
        )
        .await
    }

    /// A request authenticated only by `token`, with no Origin header (like the CLI).
    async fn bearer(&self, method: Method, path: &str, token: &str, body: Option<Value>) -> Resp {
        let b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        self.send(b, None, body).await
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn tokens_need_sudo_and_are_shown_once(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let r = app.create_token(&admin, json!({ "name": "laptop" })).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let token = r.json["token"].as_str().unwrap();
    assert!(token.starts_with("mdt_"));
    assert_eq!(r.json["name"], "laptop");
    assert!(r.json["workspaceId"].is_null());

    let list = app
        .call(Method::GET, "/api/account/tokens", Some(&admin), None)
        .await;
    assert_eq!(list.json.as_array().unwrap().len(), 1);
    assert!(list.json[0].get("token").is_none());
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM api_tokens")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_ne!(stored, token.as_bytes());

    // Bad input.
    let r = app
        .create_token(&admin, json!({ "name": "x", "expiresInDays": 0 }))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .create_token(&admin, json!({ "name": "x", "workspaceId": Id::new() }))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // Without a recent re-authentication: no.
    app.drop_sudo().await;
    let r = app.create_token(&admin, json!({ "name": "later" })).await;
    assert_eq!(r.code(), "sudo_required");
    // A token never has sudo, so it can't mint more tokens or elevate itself.
    let r = app
        .bearer(
            Method::POST,
            "/api/account/tokens",
            token,
            Some(json!({ "name": "child" })),
        )
        .await;
    assert_eq!(r.code(), "sudo_required");
    let r = app
        .bearer(
            Method::POST,
            "/api/auth/sudo",
            token,
            Some(json!({ "password": PASSWORD })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE action LIKE 'account.token%' ORDER BY id",
    )
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(actions, ["account.token_created"]);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn bearer_tokens_work_on_ordinary_routes_without_origin(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let r = app.create_token(&admin, json!({ "name": "cli" })).await;
    let token = r.json["token"].as_str().unwrap().to_owned();
    let token_id: Id = r.json["id"].as_str().unwrap().parse().unwrap();

    let me = app.bearer(Method::GET, "/api/auth/me", &token, None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json["user"]["email"], "admin@example.com");

    // A mutation with no Origin header goes through on the token alone…
    let r = app
        .bearer(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            &token,
            Some(json!({ "name": "shop" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let (by_token, by_session): (Option<Id>, Option<Id>) = sqlx::query_as(
        "SELECT actor_token_id, actor_session_id FROM audit_log WHERE action = 'project.create'",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(by_token, Some(token_id));
    assert_eq!(by_session, None);
    let used: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT last_used_at FROM api_tokens WHERE id = $1")
            .bind(token_id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(used.is_some());

    // …while a cookie request without Origin is still refused,
    let b = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/workspaces/{ws}/projects"));
    let r = app
        .send(b, Some(&admin), Some(json!({ "name": "csrf" })))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // and a bad bearer next to a valid cookie doesn't fall back to the cookie.
    let b = Request::builder()
        .method(Method::POST)
        .uri(format!("/api/workspaces/{ws}/projects"))
        .header(header::AUTHORIZATION, "Bearer mdt_nope");
    let r = app
        .send(b, Some(&admin), Some(json!({ "name": "csrf" })))
        .await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    let projects: i64 = sqlx::query_scalar("SELECT count(*) FROM projects")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(projects, 1);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn workspace_tokens_reach_only_their_workspace(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let a = app.create_workspace(&admin, "a").await;
    let b = app.create_workspace(&admin, "b").await;
    let chain_b = app.seed_chain(b).await;
    let r = app
        .create_token(&admin, json!({ "name": "ci", "workspaceId": a }))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["workspaceName"], "a");
    let token = r.json["token"].as_str().unwrap().to_owned();

    let ok = |path: String| {
        let app = &app;
        let token = token.clone();
        async move { app.bearer(Method::GET, &path, &token, None).await.status }
    };
    assert_eq!(ok(format!("/api/workspaces/{a}")).await, StatusCode::OK);
    assert_eq!(
        ok(format!("/api/workspaces/{b}")).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        ok(format!("/api/projects/{}", chain_b.project)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        ok(format!("/api/service-instances/{}", chain_b.instance)).await,
        StatusCode::NOT_FOUND
    );
    // Lists show only the token's workspace.
    let me = app.bearer(Method::GET, "/api/auth/me", &token, None).await;
    let names: Vec<&str> = me.json["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a"]);
    let list = app
        .bearer(Method::GET, "/api/workspaces", &token, None)
        .await;
    assert_eq!(list.json.as_array().unwrap().len(), 1);
    // No new workspaces, no instance administration (even for an instance admin).
    let r = app
        .bearer(
            Method::POST,
            "/api/workspaces",
            &token,
            Some(json!({ "name": "c" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(ok("/api/nodes".into()).await, StatusCode::FORBIDDEN);
    assert_eq!(
        app.bearer(
            Method::PATCH,
            &format!("/api/nodes/{}", chain_b.node),
            &token,
            Some(json!({})),
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn revoked_and_expired_tokens_are_refused(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let (_, other) = app.user("other@example.com").await;
    let r = app
        .create_token(&admin, json!({ "name": "short", "expiresInDays": 1 }))
        .await;
    let token = r.json["token"].as_str().unwrap().to_owned();
    let id = r.json["id"].as_str().unwrap().to_owned();
    assert!(r.json["expiresAt"].is_string());
    assert_eq!(
        app.bearer(Method::GET, "/api/auth/me", &token, None)
            .await
            .status,
        StatusCode::OK
    );

    sqlx::query("UPDATE api_tokens SET expires_at = now() - interval '1 second'")
        .execute(app.db())
        .await
        .unwrap();
    assert_eq!(
        app.bearer(Method::GET, "/api/auth/me", &token, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    sqlx::query("UPDATE api_tokens SET expires_at = NULL")
        .execute(app.db())
        .await
        .unwrap();

    // Someone else can't see or delete it.
    let path = format!("/api/account/tokens/{id}");
    let r = app.call(Method::DELETE, &path, Some(&other), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        app.call(Method::GET, "/api/account/tokens", Some(&other), None)
            .await
            .json,
        json!([])
    );

    let r = app.call(Method::DELETE, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        app.bearer(Method::GET, "/api/auth/me", &token, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
    let revoked: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'account.token_revoked'")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(revoked, 1);
}
