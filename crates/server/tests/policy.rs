//! Structural security tests over the whole route table. New routes are covered
//! automatically: they fail here unless they are deliberately public, reject
//! anonymous callers, and hide other workspaces' resources.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use axum::http::{Method, StatusCode};
use common::*;
use modula_server::authz::ResourceKind;
use modula_server::http::{Access, route_specs};
use sqlx::PgPool;
use uuid::Uuid;

/// Changing this list is a security decision; keep it short.
const PUBLIC_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/health"),
    ("GET", "/api/instance"),
    ("POST", "/api/auth/setup"),
    ("POST", "/api/auth/login"),
];

fn fill(path: &str, id: Uuid) -> String {
    let mut out = path.to_owned();
    for p in modula_server::http::router::path_params(path) {
        out = out.replace(&format!("{{{p}}}"), &id.to_string());
    }
    out
}

/// A resource of `kind` owned by `workspace`.
async fn fixture(_app: &TestApp, kind: ResourceKind, workspace: Uuid) -> Uuid {
    match kind {
        ResourceKind::Workspace => workspace,
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn public_routes_are_exactly_the_allowlist(pool: PgPool) {
    let specs = route_specs(common::state(pool));
    let public: BTreeSet<(String, String)> = specs
        .iter()
        .filter(|s| s.access == Access::Public)
        .map(|s| (s.method.to_string(), s.path.to_owned()))
        .collect();
    let expected: BTreeSet<(String, String)> = PUBLIC_ROUTES
        .iter()
        .map(|(m, p)| ((*m).to_owned(), (*p).to_owned()))
        .collect();
    assert_eq!(public, expected);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn every_guarded_route_rejects_anonymous_and_bad_sessions(pool: PgPool) {
    let app = TestApp::new(pool);
    let forged = modula_crypto::token::generate(modula_crypto::token::prefix::SESSION).unwrap();
    use secrecy::ExposeSecret;
    for spec in route_specs(app.state.clone())
        .iter()
        .filter(|s| s.access != Access::Public)
    {
        let path = fill(spec.path, Uuid::now_v7());
        for cookie in [
            None,
            Some("garbage"),
            Some(forged.plaintext.expose_secret()),
        ] {
            let r = app
                .call(
                    spec.method.clone(),
                    &path,
                    cookie,
                    Some(serde_json::json!({})),
                )
                .await;
            assert_eq!(
                r.status,
                StatusCode::UNAUTHORIZED,
                "{} {} with {cookie:?}",
                spec.method,
                spec.path
            );
        }
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn resource_routes_hide_other_workspaces(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let victim_ws = app.create_workspace(&admin, "victim").await;
    let (_, attacker) = app.user("attacker@example.com").await;

    let specs = route_specs(app.state.clone());
    let resource_routes: Vec<_> = specs
        .iter()
        .filter(|s| matches!(s.access, Access::Resource(_)))
        .collect();
    assert!(!resource_routes.is_empty());
    for spec in resource_routes {
        let param = modula_server::http::router::path_params(spec.path)[0];
        let kind = ResourceKind::from_param(param).unwrap();
        let id = fixture(&app, kind, victim_ws).await;
        // Body is valid-shaped but irrelevant: authz must run before extraction.
        let r = app
            .call(
                spec.method.clone(),
                &fill(spec.path, id),
                Some(&attacker),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(
            r.status,
            StatusCode::NOT_FOUND,
            "IDOR on {} {}: {:?}",
            spec.method,
            spec.path,
            r.json
        );

        // Malformed ids are a plain 404 too.
        let bad = spec.path.replace(&format!("{{{param}}}"), "not-a-uuid");
        let r = app
            .call(spec.method.clone(), &bad, Some(&attacker), None)
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }

    // Nothing was changed by the attacker.
    let name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id = $1")
        .bind(victim_ws)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(name, "victim");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn roles_are_enforced(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let (viewer_id, viewer) = app.user("viewer@example.com").await;
    app.add_member(ws, viewer_id, "viewer").await;
    let path = format!("/api/workspaces/{ws}");

    assert_eq!(
        app.call(Method::GET, &path, Some(&viewer), None)
            .await
            .status,
        StatusCode::OK
    );
    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&viewer),
            Some(serde_json::json!({"name": "pwned"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(
        app.call(Method::DELETE, &path, Some(&viewer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&admin),
            Some(serde_json::json!({"name": "renamed"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["name"], "renamed");
    assert_eq!(
        app.call(Method::DELETE, &path, Some(&admin), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        app.call(Method::GET, &path, Some(&admin), None)
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE workspace_id = $1 ORDER BY id")
            .bind(ws)
            .fetch_all(app.db())
            .await
            .unwrap();
    assert_eq!(
        actions,
        ["workspace.create", "workspace.update", "workspace.delete"]
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn instance_admin_has_no_implicit_workspace_access(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let (_, other) = app.user("other@example.com").await;
    let ws = app.create_workspace(&other, "private").await;
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[test]
fn routes_are_only_registered_through_api_router() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_files(&src, &mut files);
    for f in files {
        if f.ends_with("http/router.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&f).unwrap();
        for needle in [".route(", ".nest(", ".merge(", ".route_service("] {
            assert!(
                !text.contains(needle),
                "{} uses {needle}; register routes via ApiRouter",
                f.display()
            );
        }
    }
}
