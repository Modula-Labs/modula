//! Workspace members and invite links.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use serde_json::{Value, json};
use sqlx::PgPool;

/// Creates an invite as `cookie` and returns `(invite id, token)`.
async fn invite(app: &TestApp, cookie: &str, ws: Id, role: &str) -> (String, String) {
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/invites"),
            Some(cookie),
            Some(json!({ "role": role })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let url = r.json["url"].as_str().unwrap();
    let token = url
        .strip_prefix(&format!("{ORIGIN}/invite/"))
        .unwrap_or_else(|| panic!("unexpected url {url}"))
        .to_owned();
    (r.json["invite"]["id"].as_str().unwrap().to_owned(), token)
}

async fn members(app: &TestApp, cookie: &str, ws: Id) -> Vec<Value> {
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/members"),
            Some(cookie),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    r.json.as_array().unwrap().clone()
}

fn member_id(list: &[Value], email: &str) -> String {
    list.iter()
        .find(|m| m["email"] == email)
        .unwrap_or_else(|| panic!("{email} is not a member"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

async fn signup(app: &TestApp, token: &str, email: &str) -> common::Resp {
    app.call(
        Method::POST,
        "/api/invites/signup",
        None,
        Some(json!({ "token": token, "name": "New", "email": email, "password": PASSWORD })),
    )
    .await
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn invite_lifecycle(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let (id, token) = invite(&app, &admin, ws, "deployer").await;

    // Pending until used, with no token in the listing.
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/invites"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json.as_array().unwrap().len(), 1);
    assert_eq!(r.json[0]["id"], id.as_str());
    assert_eq!(r.json[0]["createdByName"], "Admin");
    assert!(!r.json.to_string().contains(&token));

    let r = app
        .call(
            Method::POST,
            "/api/invites/lookup",
            None,
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["workspaceName"], "team");
    assert_eq!(r.json["role"], "deployer");
    assert_eq!(r.json["invitedBy"], "Admin");

    // A new person signs up through the link and is signed in.
    let r = signup(&app, &token, "new@example.com").await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["workspaceId"], ws.to_string());
    let new = r.session_cookie().expect("signup signs in");
    let r = app
        .call(Method::GET, "/api/auth/me", Some(&new), None)
        .await;
    assert_eq!(r.json["user"]["email"], "new@example.com");
    assert_eq!(r.json["user"]["isInstanceAdmin"], false);
    assert_eq!(r.json["workspaces"][0]["role"], "deployer");

    let list = members(&app, &admin, ws).await;
    assert_eq!(list.len(), 2);
    let m = list
        .iter()
        .find(|m| m["email"] == "new@example.com")
        .unwrap();
    assert_eq!(m["role"], "deployer");
    assert_eq!(m["twoFactor"], false);

    // Single use: the link is gone everywhere.
    let r = app
        .call(
            Method::POST,
            "/api/invites/lookup",
            None,
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        signup(&app, &token, "third@example.com").await.status,
        StatusCode::NOT_FOUND
    );
    let (_, other) = app.user("other@example.com").await;
    let r = app
        .call(
            Method::POST,
            "/api/invites/accept",
            Some(&other),
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/invites"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json, json!([]));

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE workspace_id = $1 ORDER BY id")
            .bind(ws)
            .fetch_all(app.db())
            .await
            .unwrap();
    assert_eq!(
        actions,
        [
            "workspace.create",
            "invite.create",
            "auth.signup",
            "member.join"
        ]
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn signed_in_users_accept(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let (_, user) = app.user("user@example.com").await;
    let (_, token) = invite(&app, &admin, ws, "viewer").await;

    let r = app
        .call(
            Method::POST,
            "/api/invites/accept",
            Some(&user),
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["workspaceId"], ws.to_string());
    assert_eq!(
        app.call(
            Method::GET,
            &format!("/api/workspaces/{ws}"),
            Some(&user),
            None
        )
        .await
        .status,
        StatusCode::OK
    );

    // An existing member can't burn a second link.
    let (_, again) = invite(&app, &admin, ws, "admin").await;
    let r = app
        .call(
            Method::POST,
            "/api/invites/accept",
            Some(&user),
            Some(json!({ "token": again })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = app
        .call(
            Method::POST,
            "/api/invites/lookup",
            None,
            Some(json!({ "token": again })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);

    // Signing up with an email that has an account points to signing in,
    // and leaves the link usable.
    let r = signup(&app, &again, "user@example.com").await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.session_cookie().is_none());
    assert_eq!(
        signup(&app, &again, "fresh@example.com").await.status,
        StatusCode::OK
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn expired_revoked_and_unknown_links_are_rejected(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;

    let (id, token) = invite(&app, &admin, ws, "member").await;
    sqlx::query(
        "UPDATE workspace_invites SET expires_at = now() - interval '1 second' WHERE id = $1",
    )
    .bind(&id)
    .execute(app.db())
    .await
    .unwrap();
    let r = app
        .call(
            Method::POST,
            "/api/invites/lookup",
            None,
            Some(json!({ "token": token })),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(
        signup(&app, &token, "late@example.com").await.status,
        StatusCode::NOT_FOUND
    );
    // Nothing was created for the failed signup.
    let users: i64 =
        sqlx::query_scalar("SELECT count(*) FROM users WHERE email = 'late@example.com'")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(users, 0);

    let (id, token) = invite(&app, &admin, ws, "member").await;
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/invites/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        signup(&app, &token, "revoked@example.com").await.status,
        StatusCode::NOT_FOUND
    );
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/invites/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    for bad in ["garbage", "mci_short", &token.replace("mci_", "mdt_")] {
        let r = app
            .call(
                Method::POST,
                "/api/invites/lookup",
                None,
                Some(json!({ "token": bad })),
            )
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{bad}");
    }

    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/invites"),
            Some(&admin),
            Some(json!({ "role": "member", "expiresInDays": 31 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/invites"),
            Some(&admin),
            Some(json!({ "role": "owner" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn invite_endpoints_are_rate_limited(pool: PgPool) {
    let app = TestApp::new(pool);
    app.setup_admin().await;
    let mut last = StatusCode::OK;
    for _ in 0..40 {
        last = app
            .call(
                Method::POST,
                "/api/invites/lookup",
                None,
                Some(json!({ "token": "garbage" })),
            )
            .await
            .status;
        if last == StatusCode::TOO_MANY_REQUESTS {
            break;
        }
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn last_admin_is_kept(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let me = member_id(&members(&app, &admin, ws).await, "admin@example.com");

    for (method, path, body) in [
        (
            Method::PATCH,
            format!("/api/members/{me}"),
            Some(json!({ "role": "member" })),
        ),
        (Method::DELETE, format!("/api/members/{me}"), None),
        (Method::POST, format!("/api/workspaces/{ws}/leave"), None),
    ] {
        let r = app.call(method.clone(), &path, Some(&admin), body).await;
        assert_eq!(r.status, StatusCode::CONFLICT, "{method} {path}");
        assert!(
            r.json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("at least one admin")
        );
    }

    // With a second admin, the first can step down and then leave.
    let (_, token) = invite(&app, &admin, ws, "admin").await;
    let second = signup(&app, &token, "second@example.com")
        .await
        .session_cookie()
        .unwrap();
    let r = app
        .call(
            Method::PATCH,
            &format!("/api/members/{me}"),
            Some(&admin),
            Some(json!({ "role": "viewer" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["role"], "viewer");
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/leave"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        app.call(
            Method::GET,
            &format!("/api/workspaces/{ws}"),
            Some(&admin),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );

    // Now the second is the last admin.
    let them = member_id(&members(&app, &second, ws).await, "second@example.com");
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/members/{them}"),
            Some(&second),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn roles_and_removal(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let (viewer_id, viewer) = app.user("viewer@example.com").await;
    app.add_member(ws, viewer_id, "viewer").await;
    let (member_uid, member) = app.user("member@example.com").await;
    app.add_member(ws, member_uid, "member").await;

    // Everyone can see the members; only admins manage them.
    let list = members(&app, &viewer, ws).await;
    assert_eq!(list.len(), 3);
    let target = member_id(&list, "viewer@example.com");
    for cookie in [&viewer, &member] {
        let r = app
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/invites"),
                Some(cookie),
                Some(json!({ "role": "admin" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = app
            .call(
                Method::GET,
                &format!("/api/workspaces/{ws}/invites"),
                Some(cookie),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = app
            .call(
                Method::PATCH,
                &format!("/api/members/{target}"),
                Some(cookie),
                Some(json!({ "role": "admin" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
        let r = app
            .call(
                Method::DELETE,
                &format!("/api/members/{target}"),
                Some(cookie),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN);
    }

    // Role changes and removal need sudo.
    app.drop_sudo().await;
    let r = app
        .call(
            Method::PATCH,
            &format!("/api/members/{target}"),
            Some(&admin),
            Some(json!({ "role": "deployer" })),
        )
        .await;
    assert_eq!(r.code(), "sudo_required");
    let r = app
        .call(
            Method::POST,
            "/api/auth/sudo",
            Some(&admin),
            Some(json!({ "password": PASSWORD })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let admin = r.session_cookie().unwrap();

    let r = app
        .call(
            Method::PATCH,
            &format!("/api/members/{target}"),
            Some(&admin),
            Some(json!({ "role": "deployer" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["role"], "deployer");

    // Removal drops the user's tokens limited to this workspace only.
    let mut tokens = Vec::new();
    for scope in [Some(ws), None] {
        let id = Id::new();
        let t = modula_crypto::token::generate(modula_crypto::token::prefix::API_TOKEN).unwrap();
        sqlx::query(
            "INSERT INTO api_tokens (id, user_id, workspace_id, name, token_hash)
             VALUES ($1, $2, $3, 'cli', $4)",
        )
        .bind(id)
        .bind(viewer_id)
        .bind(scope)
        .bind(t.hash.as_bytes())
        .execute(app.db())
        .await
        .unwrap();
        tokens.push(id);
    }
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/members/{target}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let left: Vec<Id> = sqlx::query_scalar("SELECT id FROM api_tokens WHERE user_id = $1")
        .bind(viewer_id)
        .fetch_all(app.db())
        .await
        .unwrap();
    assert_eq!(left, [tokens[1]]);
    assert_eq!(
        app.call(
            Method::GET,
            &format!("/api/workspaces/{ws}"),
            Some(&viewer),
            None
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(members(&app, &admin, ws).await.len(), 2);

    // A membership id from another workspace is invisible.
    let other_ws = app.create_workspace(&member, "mine").await;
    let mine = member_id(
        &members(&app, &member, other_ws).await,
        "member@example.com",
    );
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/members/{mine}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE workspace_id = $1 AND action LIKE 'member.%' ORDER BY id",
    )
    .bind(ws)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert_eq!(actions, ["member.update", "member.remove"]);
}

/// An admin invite grants what promoting to admin does, so it needs sudo too.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn admin_invites_need_sudo(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    app.drop_sudo().await;
    let path = format!("/api/workspaces/{ws}/invites");
    let r = app
        .call(
            Method::POST,
            &path,
            Some(&admin),
            Some(json!({ "role": "admin" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    assert_eq!(r.json["error"]["code"], "sudo_required", "{:?}", r.json);
    let r = app
        .call(
            Method::POST,
            &path,
            Some(&admin),
            Some(json!({ "role": "member" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}
