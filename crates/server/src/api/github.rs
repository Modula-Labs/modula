//! GitHub App: creation through the manifest flow (instance admins), per-
//! workspace installations, the repository picker, and the webhook.
//!
//! Browser round trips through GitHub carry a `state` that is random,
//! single-use, short-lived, stored hashed, and bound to the user (and, for
//! installations, the workspace) that started the flow.

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Redirect;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime};
use url::Url;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::{Grant, Role};
use crate::error::{AppError, AppResult};
use crate::github::app::{self, Repo, aad};
use crate::github::push;
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// Where GitHub's web UI lives (App creation and installation pages).
const GITHUB_WEB: &str = "https://github.com";
const STATE_PREFIX: &str = "mcgh";
const MANIFEST_STATE_TTL: Duration = Duration::minutes(15);
const INSTALL_STATE_TTL: Duration = Duration::hours(1);
/// GitHub caps payloads at 25 MB, but pushes (≤ 20 commits) are far smaller.
const MAX_WEBHOOK_BODY: usize = 2 * 1024 * 1024;
/// GitHub App names are at most 34 characters.
const MAX_APP_NAME: usize = 34;
pub const WEBHOOK_PATH: &str = "/api/github/webhook";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppStatus {
    configured: bool,
    slug: Option<String>,
    html_url: Option<String>,
}

/// Whether the instance has a GitHub App (no secrets).
pub async fn status(State(state): State<AppState>) -> AppResult<Json<AppStatus>> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT slug, html_url FROM github_app")
        .fetch_optional(&state.db)
        .await?;
    Ok(Json(AppStatus {
        configured: row.is_some(),
        slug: row.as_ref().map(|r| r.0.clone()),
        html_url: row.map(|r| r.1),
    }))
}

async fn new_state(
    state: &AppState,
    kind: &str,
    user: Id,
    workspace: Option<Id>,
    ttl: Duration,
) -> AppResult<String> {
    let t = modula_crypto::token::generate(STATE_PREFIX)?;
    sqlx::query(
        "INSERT INTO github_flow_states (state_hash, kind, user_id, workspace_id, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(t.hash.as_bytes())
    .bind(kind)
    .bind(user)
    .bind(workspace)
    .bind(OffsetDateTime::now_utc() + ttl)
    .execute(&state.db)
    .await?;
    Ok(t.plaintext.expose_secret().to_owned())
}

/// Consume a state (always: a wrong user burns it too). Returns its workspace.
async fn take_state(
    state: &AppState,
    kind: &str,
    presented: &str,
    actor: &Actor,
) -> AppResult<Option<Id>> {
    let invalid = || AppError::bad_request("invalid or expired state; start again");
    if !modula_crypto::token::is_well_formed(presented, STATE_PREFIX) {
        return Err(invalid());
    }
    let row: Option<(Id, Option<Id>, bool)> = sqlx::query_as(
        "DELETE FROM github_flow_states
         WHERE state_hash = $1 AND kind = $2
         RETURNING user_id, workspace_id, expires_at > now()",
    )
    .bind(modula_crypto::token::hash(presented).as_bytes())
    .bind(kind)
    .fetch_optional(&state.db)
    .await?;
    match row {
        Some((user, ws, true)) if user == actor.user_id => Ok(ws),
        _ => Err(invalid()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestReq {
    /// Create the App under this organization instead of the admin's account.
    #[serde(default)]
    org: Option<String>,
}

#[derive(Serialize)]
pub struct ManifestResp {
    /// POST a form with a `manifest` field here.
    url: String,
    manifest: Value,
    state: String,
}

/// Start creating the instance's App: the browser POSTs `manifest` to `url`.
pub async fn manifest(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(req): Json<ManifestReq>,
) -> AppResult<Json<ManifestResp>> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM github_app)")
        .fetch_one(&state.db)
        .await?;
    if exists {
        return Err(AppError::Conflict(
            "this instance already has a GitHub App".into(),
        ));
    }
    let origin = state.cfg.public_origin();
    let host = state.cfg.public_url.host_str().unwrap_or("self-hosted");
    let mut name = format!("Modula Deploy ({host})");
    if name.chars().count() > MAX_APP_NAME {
        let keep: String = host
            .chars()
            .take(MAX_APP_NAME - "Modula Deploy (…)".chars().count())
            .collect();
        name = format!("Modula Deploy ({keep}…)");
    }
    let manifest = json!({
        "name": name,
        "url": origin,
        "hook_attributes": { "url": format!("{origin}{WEBHOOK_PATH}"), "active": true },
        "redirect_url": format!("{origin}/api/github/app/callback"),
        "setup_url": format!("{origin}/api/github/install/callback"),
        "setup_on_update": true,
        "public": false,
        "default_permissions": {
            "contents": "read",
            "metadata": "read",
            "statuses": "write",
            "deployments": "write",
            "pull_requests": "read",
        },
        "default_events": ["push", "pull_request", "installation", "installation_repositories"],
    });
    let mut url: Url = match &req.org {
        Some(org) => {
            let ok = !org.is_empty()
                && org.len() <= 39
                && !org.starts_with('-')
                && org.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
            if !ok {
                return Err(AppError::bad_request("invalid organization name"));
            }
            format!("{GITHUB_WEB}/organizations/{org}/settings/apps/new")
        }
        None => format!("{GITHUB_WEB}/settings/apps/new"),
    }
    .parse()
    .map_err(anyhow::Error::from)?;
    let st = new_state(&state, "manifest", actor.user_id, None, MANIFEST_STATE_TTL).await?;
    url.query_pairs_mut().append_pair("state", &st);
    Entry::new("github.app.manifest", &meta)
        .actor(&actor)
        .metadata(json!({ "org": req.org }))
        .record(&state.db)
        .await?;
    Ok(Json(ManifestResp {
        url: url.into(),
        manifest,
        state: st,
    }))
}

#[derive(Deserialize)]
pub struct AppCallback {
    code: String,
    state: String,
}

/// GitHub redirects here after the App is created: exchange the code for its
/// credentials and seal them. Only the admin who started the flow, in the same
/// browser, can complete it (the state is bound to them).
pub async fn app_callback(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Query(q): Query<AppCallback>,
) -> AppResult<Redirect> {
    take_state(&state, "manifest", &q.state, &actor).await?;
    if !actor.is_instance_admin {
        return Err(AppError::Forbidden);
    }
    let c = app::convert_manifest(&state, &q.code).await?;
    let webhook_secret = c
        .webhook_secret
        .ok_or_else(|| anyhow::anyhow!("GitHub returned an App without a webhook secret"))?;
    let (keys, db) = (&state.keys, &state.db);
    let private_key = keys
        .seal(db, c.pem.expose_secret().as_bytes(), aad("private_key"))
        .await?;
    let webhook = keys
        .seal(
            db,
            webhook_secret.expose_secret().as_bytes(),
            aad("webhook_secret"),
        )
        .await?;
    let client_secret = keys
        .seal(
            db,
            c.client_secret.expose_secret().as_bytes(),
            aad("client_secret"),
        )
        .await?;
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO github_app (app_id, slug, client_id, html_url, owner_login, private_key_sealed,
                                 webhook_secret_sealed, client_secret_sealed, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(c.id)
    .bind(&c.slug)
    .bind(&c.client_id)
    .bind(&c.html_url)
    .bind(c.owner.as_ref().map(|o| &o.login))
    .bind(&private_key)
    .bind(&webhook)
    .bind(&client_secret)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(AppError::Conflict(
            "this instance already has a GitHub App".into(),
        ));
    }
    Entry::new("github.app.create", &meta)
        .actor(&actor)
        .target("github_app", c.id)
        .metadata(json!({ "slug": c.slug }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to("/settings?github=created"))
}

#[derive(Serialize)]
pub struct InstallResp {
    url: String,
}

/// Where to send the browser to install the App for this workspace.
pub async fn install(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
) -> AppResult<Json<InstallResp>> {
    let html_url: Option<String> = sqlx::query_scalar("SELECT html_url FROM github_app")
        .fetch_optional(&state.db)
        .await?;
    let html_url = html_url.ok_or_else(|| {
        AppError::Conflict("an instance admin must set up the GitHub App first".into())
    })?;
    let mut url: Url = format!("{}/installations/new", html_url.trim_end_matches('/'))
        .parse()
        .map_err(anyhow::Error::from)?;
    let st = new_state(
        &state,
        "install",
        actor.user_id,
        Some(grant.workspace_id()?),
        INSTALL_STATE_TTL,
    )
    .await?;
    url.query_pairs_mut().append_pair("state", &st);
    Ok(Json(InstallResp { url: url.into() }))
}

#[derive(Deserialize)]
pub struct InstallCallback {
    installation_id: Option<i64>,
    setup_action: Option<String>,
    state: Option<String>,
}

/// GitHub redirects here after an installation: verify it with the App and
/// bind it to the workspace the flow was started from.
pub async fn install_callback(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Query(q): Query<InstallCallback>,
) -> AppResult<Redirect> {
    // An org owner must approve; GitHub calls back again once they do.
    if q.setup_action.as_deref() == Some("request") {
        return Ok(Redirect::to("/settings?github=requested"));
    }
    // Changes made on GitHub (repository selection) arrive without our state;
    // there is nothing to record for them.
    let Some(presented) = q.state.as_deref() else {
        return Ok(Redirect::to("/settings?github=updated"));
    };
    let workspace = take_state(&state, "install", presented, &actor)
        .await?
        .ok_or_else(|| anyhow::anyhow!("install state without a workspace"))?;
    let id = q
        .installation_id
        .filter(|i| *i > 0)
        .ok_or_else(|| AppError::bad_request("missing installation_id"))?;
    // Still an admin of that workspace?
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace)
    .bind(actor.user_id)
    .fetch_optional(&state.db)
    .await?;
    if role.as_deref().and_then(Role::parse) < Some(Role::Admin) {
        return Err(AppError::Forbidden);
    }
    let inst = app::installation(&state, id).await?;
    let mut tx = state.db.begin().await?;
    // Re-installing refreshes the account; it never moves to another workspace.
    let bound = sqlx::query(
        "INSERT INTO github_installations (id, workspace_id, account_login, account_type, created_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (id) DO UPDATE
           SET account_login = EXCLUDED.account_login, account_type = EXCLUDED.account_type
           WHERE github_installations.workspace_id = EXCLUDED.workspace_id",
    )
    .bind(id)
    .bind(workspace)
    .bind(&inst.account_login)
    .bind(&inst.account_type)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if bound == 0 {
        return Err(AppError::Conflict(
            "this GitHub installation belongs to another workspace".into(),
        ));
    }
    Entry::new("github.install", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("github_installation", id)
        .metadata(json!({ "account": inst.account_login }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Redirect::to("/settings?github=installed"))
}

/// Repositories the workspace's installations can reach (the repo picker).
pub async fn repos(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Vec<Repo>>> {
    Ok(Json(
        app::workspace_repos(&state, grant.workspace_id()?).await?,
    ))
}

/// GitHub webhook deliveries, authenticated by HMAC under the App's secret.
pub async fn webhook(
    State(state): State<AppState>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: Body,
) -> AppResult<(StatusCode, Json<Value>)> {
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    let sealed: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT webhook_secret_sealed FROM github_app")
            .fetch_optional(&state.db)
            .await?;
    let (Some(sealed), Some(signature)) = (sealed, header("x-hub-signature-256")) else {
        return Err(AppError::Unauthorized);
    };
    let body = axum::body::to_bytes(body, MAX_WEBHOOK_BODY)
        .await
        .map_err(|_| AppError::bad_request("payload too large"))?;
    let secret = state
        .keys
        .open(&state.db, &sealed, aad("webhook_secret"))
        .await?;
    if !push::verify_signature(&secret, &body, Some(signature)) {
        tracing::warn!("webhook with a bad signature");
        return Err(AppError::Unauthorized);
    }
    let delivery = header("x-github-delivery")
        .filter(|d| !d.is_empty() && d.len() <= 100)
        .ok_or_else(|| AppError::bad_request("missing X-GitHub-Delivery"))?
        .to_owned();
    let event = header("x-github-event").unwrap_or_default().to_owned();
    let fresh =
        sqlx::query("INSERT INTO github_deliveries (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(&delivery)
            .execute(&state.db)
            .await?
            .rows_affected();
    if fresh == 0 {
        return Ok((StatusCode::OK, Json(json!({ "duplicate": true }))));
    }
    let res = handle_event(&state, &meta, &delivery, &event, &body).await;
    if matches!(res, Err(AppError::Internal(_))) {
        // Let GitHub's redelivery try again.
        let _ = sqlx::query("DELETE FROM github_deliveries WHERE id = $1")
            .bind(&delivery)
            .execute(&state.db)
            .await;
    }
    res
}

async fn handle_event(
    state: &AppState,
    meta: &RequestMeta,
    delivery: &str,
    event: &str,
    body: &[u8],
) -> AppResult<(StatusCode, Json<Value>)> {
    let bad = |e: serde_json::Error| AppError::bad_request(format!("invalid {event} payload: {e}"));
    match event {
        "ping" => Ok((StatusCode::OK, Json(json!({})))),
        "push" => {
            let ev: push::PushEvent = serde_json::from_slice(body).map_err(bad)?;
            let ids = push::handle_push(state, meta, delivery, &ev).await?;
            Ok((StatusCode::OK, Json(json!({ "deployments": ids }))))
        }
        "installation" | "installation_repositories" => {
            #[derive(Deserialize)]
            struct Ev {
                action: String,
                installation: push::InstallationRef,
            }
            let ev: Ev = serde_json::from_slice(body).map_err(bad)?;
            let id = ev.installation.id;
            // Cached tokens may cover repositories that were just removed.
            app::forget_installation(state, id);
            if event == "installation" && ev.action == "deleted" {
                let mut tx = state.db.begin().await?;
                let ws: Option<Id> = sqlx::query_scalar(
                    "DELETE FROM github_installations WHERE id = $1 RETURNING workspace_id",
                )
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
                if let Some(ws) = ws {
                    Entry::new("github.uninstall", meta)
                        .workspace(ws)
                        .target("github_installation", id)
                        .record(&mut *tx)
                        .await?;
                }
                tx.commit().await?;
            }
            Ok((StatusCode::OK, Json(json!({}))))
        }
        _ => Ok((StatusCode::ACCEPTED, Json(json!({})))),
    }
}

/// Forget old webhook deliveries and abandoned flow states (hourly cleanup).
pub async fn prune(db: &sqlx::PgPool) -> AppResult<()> {
    sqlx::query("DELETE FROM github_deliveries WHERE received_at < now() - interval '7 days'")
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM github_flow_states WHERE expires_at < now()")
        .execute(db)
        .await?;
    Ok(())
}
