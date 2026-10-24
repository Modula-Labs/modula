//! Structural security tests over the whole route table. New routes are covered
//! automatically: they fail here unless they are deliberately public, reject
//! anonymous callers, and hide other workspaces' resources.

mod common;

use std::collections::BTreeSet;
use std::path::Path;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_server::authz::ResourceKind;
use modula_server::http::{Access, route_specs};
use sqlx::PgPool;

/// Changing this list is a security decision; keep it short.
const PUBLIC_ROUTES: &[(&str, &str)] = &[
    ("GET", "/api/health"),
    ("GET", "/api/instance"),
    ("GET", "/api/install.sh"),
    ("GET", "/api/releases"),
    ("POST", "/api/auth/setup"),
    ("POST", "/api/auth/login"),
    // Authenticated by the single-use invite token in the body; rate-limited.
    ("POST", "/api/invites/lookup"),
    ("POST", "/api/invites/signup"),
    // Authenticated by an HMAC signature under the GitHub App's webhook secret.
    ("POST", "/api/github/webhook"),
    // Nodes downloading an upload they build: a signed token bound to a
    // deployment that must still be building it (deploy::upload).
    ("GET", "/api/build-source"),
    // Nodes uploading build output hosted elsewhere (a static site, or a
    // function's bundle): a signed token bound to a deployment that must
    // still be building (deploy::output).
    ("PUT", "/api/build-output"),
];

fn fill(path: &str, id: Id) -> String {
    let mut out = path.to_owned();
    for p in modula_server::http::router::path_params(path) {
        out = out.replace(&format!("{{{p}}}"), &id.to_string());
    }
    out
}

/// A resource of `kind` owned by `workspace` (or by `victim`, for account kinds).
async fn fixture(
    app: &TestApp,
    kind: ResourceKind,
    workspace: Id,
    victim: Id,
    chain: &Chain,
) -> Id {
    match kind {
        ResourceKind::Workspace => workspace,
        ResourceKind::Session => {
            sqlx::query_scalar("SELECT id FROM sessions WHERE user_id = $1 LIMIT 1")
                .bind(victim)
                .fetch_one(app.db())
                .await
                .unwrap()
        }
        ResourceKind::Project => chain.project,
        ResourceKind::Environment => chain.environment,
        ResourceKind::Service => chain.service,
        ResourceKind::ServiceInstance => chain.instance,
        ResourceKind::Variable => chain.variable,
        ResourceKind::Deployment => chain.deployment,
        ResourceKind::Domain => chain.domain,
        ResourceKind::Node => chain.node,
        ResourceKind::BackupTarget => chain.backup_target,
        ResourceKind::Backup => chain.backup,
        ResourceKind::Volume => chain.volume,
        ResourceKind::TcpProxy => chain.tcp_proxy,
        ResourceKind::ProvisionedServer => {
            let id = Id::new();
            sqlx::query(
                "INSERT INTO provisioned_servers (id, workspace_id, provider, name, location, server_type, monthly_price_cents)
                 VALUES ($1, $2, 'hetzner', 'worker', 'hetzner:fsn1', 'cx22', 452)",
            )
            .bind(id)
            .bind(workspace)
            .execute(app.db())
            .await
            .unwrap();
            id
        }
        ResourceKind::Webhook => {
            let id = Id::new();
            sqlx::query(
                "INSERT INTO webhooks (id, project_id, url, events, secret_sealed)
                 VALUES ($1, $2, 'https://example.com/hook', '{deployment.failed}', '\\x00')",
            )
            .bind(id)
            .bind(chain.project)
            .execute(app.db())
            .await
            .unwrap();
            id
        }
        ResourceKind::Member => {
            sqlx::query_scalar("SELECT id FROM workspace_members WHERE workspace_id = $1 LIMIT 1")
                .bind(workspace)
                .fetch_one(app.db())
                .await
                .unwrap()
        }
        ResourceKind::Invite => {
            let id = Id::new();
            let t = modula_crypto::token::generate(modula_crypto::token::prefix::INVITE).unwrap();
            sqlx::query(
                "INSERT INTO workspace_invites (id, workspace_id, role, token_hash, expires_at)
                 VALUES ($1, $2, 'member', $3, now() + interval '1 day')",
            )
            .bind(id)
            .bind(workspace)
            .bind(t.hash.as_bytes())
            .execute(app.db())
            .await
            .unwrap();
            id
        }
        ResourceKind::NotificationChannel => {
            let id = Id::new();
            sqlx::query(
                "INSERT INTO notification_channels (id, workspace_id, kind, name, host, url_sealed)
                 VALUES ($1, $2, 'slack', 'ops', 'hooks.slack.com', '\\x00')",
            )
            .bind(id)
            .bind(workspace)
            .execute(app.db())
            .await
            .unwrap();
            id
        }
        ResourceKind::AlertRule => {
            let id = Id::new();
            sqlx::query(
                "INSERT INTO alert_rules (id, workspace_id, name, kind) VALUES ($1, $2, 'r', 'deployment_failed')",
            )
            .bind(id)
            .bind(workspace)
            .execute(app.db())
            .await
            .unwrap();
            id
        }
        ResourceKind::ApiToken => {
            let id = Id::new();
            let t =
                modula_crypto::token::generate(modula_crypto::token::prefix::API_TOKEN).unwrap();
            sqlx::query(
                "INSERT INTO api_tokens (id, user_id, name, token_hash) VALUES ($1, $2, 'cli', $3)",
            )
            .bind(id)
            .bind(victim)
            .bind(t.hash.as_bytes())
            .execute(app.db())
            .await
            .unwrap();
            id
        }
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
        let path = fill(spec.path, Id::new());
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
        // Bad API tokens: unknown, malformed, and a session token as a bearer.
        let unknown = modula_crypto::token::generate(modula_crypto::token::prefix::API_TOKEN)
            .unwrap()
            .plaintext;
        for auth in [
            format!("Bearer {}", unknown.expose_secret()),
            "Bearer garbage".to_owned(),
            format!("Bearer {}", forged.plaintext.expose_secret()),
            "Basic dXNlcjpwdw==".to_owned(),
        ] {
            let b = axum::http::Request::builder()
                .method(spec.method.clone())
                .uri(&path)
                .header(axum::http::header::AUTHORIZATION, &auth);
            let r = app.send(b, None, Some(serde_json::json!({}))).await;
            assert_eq!(
                r.status,
                StatusCode::UNAUTHORIZED,
                "{} {} with {auth}",
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
    let victim: Id = sqlx::query_scalar("SELECT id FROM users WHERE email = 'admin@example.com'")
        .fetch_one(app.db())
        .await
        .unwrap();
    let (_, attacker) = app.user("attacker@example.com").await;
    let chain = app.seed_chain(victim_ws).await;

    let specs = route_specs(app.state.clone());
    let resource_routes: Vec<_> = specs
        .iter()
        .filter(|s| matches!(s.access, Access::Resource(_)))
        .collect();
    assert!(!resource_routes.is_empty());
    for spec in resource_routes {
        let param = modula_server::http::router::path_params(spec.path)[0];
        let kind = ResourceKind::from_param(param).unwrap();
        let id = fixture(&app, kind, victim_ws, victim, &chain).await;
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
        let bad = spec.path.replace(&format!("{{{param}}}"), "not-an-id");
        let r = app
            .call(spec.method.clone(), &bad, Some(&attacker), None)
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
    }

    // Nothing was changed by the attacker, and the victim is still signed in.
    assert_eq!(
        app.call(Method::GET, "/api/auth/me", Some(&admin), None)
            .await
            .status,
        StatusCode::OK
    );
    let name: String = sqlx::query_scalar("SELECT name FROM workspaces WHERE id = $1")
        .bind(victim_ws)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(name, "victim");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn instance_routes_are_admin_only(pool: PgPool) {
    let app = TestApp::new(pool);
    app.setup_admin().await;
    let (_, user) = app.user("user@example.com").await;
    let specs = route_specs(app.state.clone());
    let instance_routes: Vec<_> = specs
        .iter()
        .filter(|s| matches!(s.access, Access::Instance(_)))
        .collect();
    assert!(!instance_routes.is_empty());
    for spec in instance_routes {
        let r = app
            .call(
                spec.method.clone(),
                spec.path,
                Some(&user),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(
            r.status,
            StatusCode::FORBIDDEN,
            "{} {}",
            spec.method,
            spec.path
        );
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn sudo_routes_require_recent_reauthentication(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let me: Id = sqlx::query_scalar("SELECT id FROM users WHERE email = 'admin@example.com'")
        .fetch_one(app.db())
        .await
        .unwrap();
    let chain = app.seed_chain(ws).await;
    app.drop_sudo().await;
    let specs = route_specs(app.state.clone());
    // Cookie sessions need sudo for session-sudo routes too (tokens pass those).
    let sudo_routes: Vec<_> = specs
        .iter()
        .filter(|s| s.access.requires_sudo() || s.access.session_sudo())
        .collect();
    assert!(
        sudo_routes.len() >= 6,
        "expected sudo-gated routes, got {}",
        sudo_routes.len()
    );
    for spec in sudo_routes {
        let path = match modula_server::http::router::path_params(spec.path).first() {
            Some(p) => {
                let kind = ResourceKind::from_param(p).unwrap();
                fill(spec.path, fixture(&app, kind, ws, me, &chain).await)
            }
            None => spec.path.to_owned(),
        };
        let r = app
            .call(
                spec.method.clone(),
                &path,
                Some(&admin),
                Some(serde_json::json!({})),
            )
            .await;
        assert_eq!(
            r.status,
            StatusCode::FORBIDDEN,
            "{} {}",
            spec.method,
            spec.path
        );
        assert_eq!(r.code(), "sudo_required", "{} {}", spec.method, spec.path);
    }
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

/// A function's code runs on the servers with the environment's variables:
/// only members and above write it; viewers and deployers read it.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn function_code_is_written_by_members_only(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let p = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(&admin),
            Some(serde_json::json!({"name": "api"})),
        )
        .await;
    let project = p.json["id"].as_str().unwrap().to_owned();
    let config = |code: &str| {
        serde_json::json!({ "source": { "type": "function", "code": code, "compatibilityDate": "2026-10-01" } })
    };
    let s = app
        .call(
            Method::POST,
            &format!("/api/projects/{project}/services"),
            Some(&admin),
            Some(serde_json::json!({ "name": "hello", "config": config("export default {}") })),
        )
        .await;
    assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
    let instance = s.json["instances"][0]["id"].as_str().unwrap().to_owned();
    let path = format!("/api/service-instances/{instance}");
    let evil = serde_json::json!({ "config": config("export default { fetch() { return fetch('http://169.254.169.254/') } }") });
    for (email, role, writes) in [
        ("viewer@example.com", "viewer", false),
        ("deployer@example.com", "deployer", false),
        ("member@example.com", "member", true),
    ] {
        let (id, cookie) = app.user(email).await;
        app.add_member(ws, id, role).await;
        let r = app.call(Method::GET, &path, Some(&cookie), None).await;
        assert_eq!(r.status, StatusCode::OK, "{role} reads it");
        assert_eq!(r.json["config"]["source"]["type"], "function");
        let r = app
            .call(Method::PATCH, &path, Some(&cookie), Some(evil.clone()))
            .await;
        let want = if writes { StatusCode::OK } else { StatusCode::FORBIDDEN };
        assert_eq!(r.status, want, "{role}: {:?}", r.json);
        let r = app
            .call(
                Method::POST,
                &format!("/api/projects/{project}/services"),
                Some(&cookie),
                Some(serde_json::json!({ "name": format!("fn-{role}"), "config": config("export default {}") })),
            )
            .await;
        assert_eq!(r.status, want, "{role} creating one: {:?}", r.json);
    }
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
