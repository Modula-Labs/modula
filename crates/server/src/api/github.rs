//! GitHub Apps: creation through the manifest flow (instance admins), per-
//! workspace installations, the repository picker, and the webhook.
//!
//! An instance normally has one App, created under the admin's GitHub account
//! (or an organization) and public, so it installs on any account or
//! organization: GitHub's own installation page offers the user's accounts to
//! pick from, nothing has to be typed. Older instances may have private Apps,
//! one per account; they keep working.
//!
//! Public means strangers can install the App too. That gives them nothing:
//! an installation is used only once it is bound to a workspace, and it binds
//! only through our install callback, with a `state` this instance issued to
//! a workspace admin and proof (OAuth on install) that the GitHub user coming
//! back can access that installation. Webhooks for installations that aren't
//! bound are acknowledged and dropped.
//!
//! "Connect GitHub" is one browser round trip: when there is no App for the
//! account yet, an instance admin creates one through the manifest flow and its
//! callback carries straight on to installing it on the workspace; otherwise
//! the flow starts at the App's installation page. Either way the user lands
//! back where they started (`return_to`), with `?github=installed`.
//!
//! Every leg carries a `state` that is random, single-use, short-lived, stored
//! hashed, and bound to the user (and workspace, and return path) that started
//! the flow, so it can't be forged, replayed, or completed by someone else.

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
use crate::github::app::{self, Branch, Repo, aad};
use crate::github::{ci, pull_request, push};
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
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
/// Webhook deliveries per source IP, checked before any work (GitHub sends
/// from a handful of addresses; a busy org stays far below this).
const WEBHOOK_PER_IP: Limit = Limit::per_minute(120);
/// Installation callbacks per user (each costs GitHub API calls).
const INSTALL_CALLBACK_PER_USER: Limit = Limit::per_minute(10);

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AppView {
    id: i64,
    slug: String,
    html_url: String,
    /// The account or organization that owns the App.
    owner_login: Option<String>,
    public: bool,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
    /// Permissions (`name: level`) and events the App lacks: created by an
    /// older version, its owner has to approve them on GitHub.
    missing_permissions: Vec<String>,
    missing_events: Vec<String>,
    /// Where the App's owner reviews its permissions and events.
    #[sqlx(skip)]
    permissions_url: String,
    #[serde(skip)]
    owner_is_org: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppList {
    apps: Vec<AppView>,
    /// Where GitHub delivers webhooks: this instance's public URL.
    webhook_url: String,
}

/// The instance's GitHub Apps (no secrets).
pub async fn apps(State(state): State<AppState>) -> AppResult<Json<AppList>> {
    let mut apps: Vec<AppView> = sqlx::query_as(
        "SELECT a.id, a.slug, a.html_url, a.owner_login, a.public, a.created_at,
                a.missing_permissions, a.missing_events,
                EXISTS (SELECT 1 FROM github_installations g
                        WHERE g.app_id = a.id AND g.account_type = 'Organization'
                          AND lower(g.account_login) = lower(a.owner_login)) AS owner_is_org
         FROM github_apps a ORDER BY a.created_at",
    )
    .fetch_all(&state.db)
    .await?;
    let web = app::web_origin(&state);
    let web = web.as_str().trim_end_matches('/');
    for a in &mut apps {
        a.permissions_url = match (&a.owner_login, a.owner_is_org) {
            (Some(org), true) => format!(
                "{web}/organizations/{org}/settings/apps/{}/permissions",
                a.slug
            ),
            _ => format!("{web}/settings/apps/{}/permissions", a.slug),
        };
    }
    Ok(Json(AppList {
        apps,
        webhook_url: format!("{}{WEBHOOK_PATH}", state.public_origin()),
    }))
}

/// Remove an App from the instance: its installations stop working in every
/// workspace (the App itself stays on GitHub until deleted there).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteAppReq {
    id: i64,
}

pub async fn delete_app(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Json(DeleteAppReq { id }): Json<DeleteAppReq>,
) -> AppResult<StatusCode> {
    let mut tx = state.db.begin().await?;
    let slug: Option<String> =
        sqlx::query_scalar("DELETE FROM github_apps WHERE id = $1 RETURNING slug")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    let slug = slug.ok_or(AppError::NotFound)?;
    Entry::new("github.app.delete", &meta)
        .actor(&actor)
        .target("github_app", id)
        .metadata(json!({ "slug": slug }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    app::forget_tokens(&state);
    app::forget_lists(&state);
    Ok(StatusCode::NO_CONTENT)
}

/// What a flow state remembers besides its user.
#[derive(Debug, Default)]
struct Flow {
    /// The workspace to install the App on.
    workspace: Option<Id>,
    /// The App being installed.
    app: Option<i64>,
    /// The dashboard path to return to (see `return_path`).
    return_to: Option<String>,
}

async fn new_state(
    state: &AppState,
    kind: &str,
    user: Id,
    flow: &Flow,
    ttl: Duration,
) -> AppResult<String> {
    let t = modula_crypto::token::generate(STATE_PREFIX)?;
    sqlx::query(
        "INSERT INTO github_flow_states (state_hash, kind, user_id, workspace_id, app_id, return_to, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(t.hash.as_bytes())
    .bind(kind)
    .bind(user)
    .bind(flow.workspace)
    .bind(flow.app)
    .bind(&flow.return_to)
    .bind(OffsetDateTime::now_utc() + ttl)
    .execute(&state.db)
    .await?;
    Ok(t.plaintext.expose_secret().to_owned())
}

/// Consume a state (always: a wrong user burns it too).
async fn take_state(
    state: &AppState,
    kind: &str,
    presented: &str,
    actor: &Actor,
) -> AppResult<Flow> {
    let invalid = || AppError::bad_request("invalid or expired state; start again");
    if !modula_crypto::token::is_well_formed(presented, STATE_PREFIX) {
        return Err(invalid());
    }
    #[derive(sqlx::FromRow)]
    struct Row {
        user_id: Id,
        workspace_id: Option<Id>,
        app_id: Option<i64>,
        return_to: Option<String>,
        live: bool,
    }
    let row: Option<Row> = sqlx::query_as(
        "DELETE FROM github_flow_states
         WHERE state_hash = $1 AND kind = $2
         RETURNING user_id, workspace_id, app_id, return_to, expires_at > now() AS live",
    )
    .bind(modula_crypto::token::hash(presented).as_bytes())
    .bind(kind)
    .fetch_optional(&state.db)
    .await?;
    match row {
        Some(r) if r.live && r.user_id == actor.user_id => Ok(Flow {
            workspace: r.workspace_id,
            app: r.app_id,
            return_to: r.return_to,
        }),
        _ => Err(invalid()),
    }
}

/// A dashboard path to come back to: same origin only (`/…`, never `//host`
/// or a backslash variant browsers treat alike), so it can't redirect away.
fn return_path(path: Option<String>) -> AppResult<Option<String>> {
    let Some(path) = path else { return Ok(None) };
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && path.len() <= 512
        && !path.contains('\\')
        && !path.chars().any(char::is_control);
    if !ok {
        return Err(AppError::bad_request(
            "returnTo must be a path on this site",
        ));
    }
    Ok(Some(path))
}

/// `return_to` (or the settings page) with `github=<outcome>` added to its query.
fn back_to(return_to: Option<&str>, outcome: &str) -> Redirect {
    let base: Url = "http://dashboard.invalid".parse().expect("static URL");
    let mut url = return_to
        .and_then(|p| base.join(p).ok())
        .filter(|u| u.host() == base.host())
        .unwrap_or_else(|| base.join("/settings").expect("static path"));
    url.query_pairs_mut().append_pair("github", outcome);
    let mut path = url.path().to_owned();
    if let Some(q) = url.query() {
        path = format!("{path}?{q}");
    }
    if let Some(f) = url.fragment() {
        path = format!("{path}#{f}");
    }
    Redirect::to(&path)
}

/// Whether `user` may connect GitHub to `workspace` (workspace admins).
async fn workspace_admin(state: &AppState, workspace: Id, user: Id) -> AppResult<bool> {
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM workspace_members WHERE workspace_id = $1 AND user_id = $2",
    )
    .bind(workspace)
    .bind(user)
    .fetch_optional(&state.db)
    .await?;
    Ok(role.as_deref().and_then(Role::parse) >= Some(Role::Admin))
}

/// The App's installation page, with a fresh `install` state for `flow`.
async fn install_url(state: &AppState, html_url: &str, user: Id, flow: &Flow) -> AppResult<String> {
    // select_target: GitHub's account picker (personal account or any org).
    let mut url: Url = format!(
        "{}/installations/select_target",
        html_url.trim_end_matches('/')
    )
    .parse()
    .map_err(anyhow::Error::from)?;
    let st = new_state(state, "install", user, flow, INSTALL_STATE_TTL).await?;
    url.query_pairs_mut().append_pair("state", &st);
    Ok(url.into())
}

/// The App's name: `Modula Deploy (<host>)`, cut to GitHub's 34 characters,
/// with `suffix` appended when the plain name is taken.
fn app_name(host: &str, suffix: Option<&str>) -> String {
    let tail = suffix.map(|s| format!(" {s}")).unwrap_or_default();
    let fixed = "Modula Deploy ()".chars().count() + tail.chars().count();
    let host = if fixed + host.chars().count() > MAX_APP_NAME {
        let keep: String = host.chars().take(MAX_APP_NAME - fixed - 1).collect();
        format!("{keep}…")
    } else {
        host.to_owned()
    };
    format!("Modula Deploy ({host}){tail}")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ManifestReq {
    /// Create the App under this organization instead of the admin's account
    /// (ownership only: it is public like any new App).
    #[serde(default)]
    org: Option<String>,
    /// Install the new App on this workspace right after creating it.
    #[serde(default)]
    workspace_id: Option<Id>,
    /// Where to come back to once it's installed (a dashboard path).
    #[serde(default)]
    return_to: Option<String>,
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
    if let Some(org) = &req.org {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM github_apps WHERE lower(owner_login) = lower($1))",
        )
        .bind(org)
        .fetch_one(&state.db)
        .await?;
        if exists {
            return Err(AppError::Conflict(format!(
                "there already is a GitHub App for {org}; install that one"
            )));
        }
    } else {
        // A public App installs on any account: one is all an instance needs.
        let public: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM github_apps WHERE public)")
                .fetch_one(&state.db)
                .await?;
        if public {
            return Err(AppError::Conflict(
                "this instance already has a GitHub App that installs on any account; \
                 install that one"
                    .into(),
            ));
        }
    }
    let flow = Flow {
        workspace: req.workspace_id,
        app: None,
        return_to: return_path(req.return_to)?,
    };
    // Checked now, so an instance admin who can't install on the workspace
    // doesn't find out only after the round trip through GitHub.
    if let Some(ws) = flow.workspace
        && !workspace_admin(&state, ws, actor.user_id).await?
    {
        return Err(AppError::Forbidden);
    }
    let origin = state.public_origin();
    let public_url = state.public_url();
    let host = public_url.host_str().unwrap_or("self-hosted");
    // App names are global on GitHub: another instance may have this one.
    let mut name = app_name(host, None);
    if app::app_name_taken(&state, &name).await {
        let suffix = hex::encode(modula_crypto::random_bytes::<2>()?);
        name = app_name(host, Some(&suffix));
    }
    // Least privilege: exactly what installation tokens ask for (app::TokenScope).
    // GitHub always sends Apps their installation and installation_repositories events.
    let manifest = json!({
        "name": name,
        "url": origin,
        "hook_attributes": { "url": format!("{origin}{WEBHOOK_PATH}"), "active": true },
        "redirect_url": format!("{origin}/api/github/app/callback"),
        "setup_url": format!("{origin}/api/github/install/callback"),
        "setup_on_update": true,
        // The installing user authorizes the App on the way back, so the
        // callback can prove they can access the installation it names.
        "request_oauth_on_install": true,
        "callback_urls": [format!("{origin}/api/github/install/callback")],
        // Public: installable on any account or organization, so one App
        // covers all of them and GitHub's installation page offers the
        // user's accounts to pick from. Strangers can install it too, but
        // an installation binds to a workspace only through install_callback
        // (our state + the user's proof of access), and nothing else uses
        // installations that aren't bound.
        "public": true,
        "default_permissions": app::PERMISSIONS
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect::<serde_json::Map<_, _>>(),
        "default_events": app::EVENTS,
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
    let st = new_state(&state, "manifest", actor.user_id, &flow, MANIFEST_STATE_TTL).await?;
    url.query_pairs_mut().append_pair("state", &st);
    Entry::new("github.app.manifest", &meta)
        .actor(&actor)
        .metadata(json!({ "org": req.org, "workspace": flow.workspace }))
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
/// browser, can complete it (the state is bound to them). Started from a
/// workspace, it carries straight on to installing the App there.
pub async fn app_callback(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Query(q): Query<AppCallback>,
) -> AppResult<Redirect> {
    let flow = take_state(&state, "manifest", &q.state, &actor).await?;
    if !actor.is_instance_admin {
        return Err(AppError::Forbidden);
    }
    let c = app::convert_manifest(&state, &q.code).await?;
    // Stored and later used as a link and redirect target: GitHub's own web
    // host only.
    if !app::is_github_web_url(&state, &c.html_url) {
        return Err(AppError::Unavailable(
            "GitHub returned an unexpected App URL; try again".into(),
        ));
    }
    let webhook_secret = c
        .webhook_secret
        .ok_or_else(|| anyhow::anyhow!("GitHub returned an App without a webhook secret"))?;
    let (keys, db) = (&state.keys, &state.db);
    let row = c.id.to_string();
    let private_key = keys
        .seal(
            db,
            c.pem.expose_secret().as_bytes(),
            aad("private_key", &row),
        )
        .await?;
    let webhook = keys
        .seal(
            db,
            webhook_secret.expose_secret().as_bytes(),
            aad("webhook_secret", &row),
        )
        .await?;
    let client_secret = keys
        .seal(
            db,
            c.client_secret.expose_secret().as_bytes(),
            aad("client_secret", &row),
        )
        .await?;
    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO github_apps (id, slug, client_id, html_url, owner_login, public,
                                  private_key_sealed, webhook_secret_sealed,
                                  client_secret_sealed, created_by)
         VALUES ($1, $2, $3, $4, $5, true, $6, $7, $8, $9)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(c.id)
    .bind(&c.slug)
    .bind(&c.client_id)
    .bind(&c.html_url)
    .bind(c.owner.as_ref().map(|o| &o.login))
    // `public = true` above: what the manifest asked for (GitHub's conversion
    // response doesn't echo it).
    .bind(&private_key)
    .bind(&webhook)
    .bind(&client_secret)
    .bind(actor.user_id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Err(AppError::Conflict(
            "this GitHub App is already set up".into(),
        ));
    }
    Entry::new("github.app.create", &meta)
        .actor(&actor)
        .target("github_app", c.id)
        .metadata(json!({ "slug": c.slug }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;
    if flow.workspace.is_some() {
        let flow = Flow {
            app: Some(c.id),
            ..flow
        };
        let url = install_url(&state, &c.html_url, actor.user_id, &flow).await?;
        return Ok(Redirect::to(&url));
    }
    Ok(back_to(flow.return_to.as_deref(), "created"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct InstallReq {
    /// Where to come back to once it's installed (a dashboard path).
    #[serde(default)]
    return_to: Option<String>,
    /// Which App to install (needed when the instance has several).
    #[serde(default)]
    app_id: Option<i64>,
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
    Json(req): Json<InstallReq>,
) -> AppResult<Json<InstallResp>> {
    let apps: Vec<(i64, String)> = sqlx::query_as(
        "SELECT id, html_url FROM github_apps WHERE $1::bigint IS NULL OR id = $1 LIMIT 2",
    )
    .bind(req.app_id)
    .fetch_all(&state.db)
    .await?;
    let (app_id, html_url) = match (apps.as_slice(), req.app_id) {
        ([], None) => {
            return Err(AppError::Conflict(
                "an instance admin must add a GitHub App first".into(),
            ));
        }
        ([], Some(_)) => return Err(AppError::NotFound),
        ([one], _) => one.clone(),
        _ => {
            return Err(AppError::Conflict(
                "this instance has several GitHub Apps; choose one (appId)".into(),
            ));
        }
    };
    let flow = Flow {
        workspace: Some(grant.workspace_id()?),
        app: Some(app_id),
        return_to: return_path(req.return_to)?,
    };
    let url = install_url(&state, &html_url, actor.user_id, &flow).await?;
    Ok(Json(InstallResp { url }))
}

#[derive(Deserialize)]
pub struct InstallCallback {
    installation_id: Option<i64>,
    setup_action: Option<String>,
    state: Option<String>,
    /// From the App's OAuth-on-install step: proves which installations the
    /// returning GitHub user can access.
    code: Option<String>,
}

/// GitHub redirects here after an installation: check that the GitHub user
/// coming back can access it and that it is our App's, then bind it to the
/// workspace the flow was started from.
pub async fn install_callback(
    State(state): State<AppState>,
    actor: Actor,
    meta: RequestMeta,
    Query(q): Query<InstallCallback>,
) -> AppResult<Redirect> {
    // Changes made on GitHub (repository selection) arrive without our state;
    // there is nothing to record for them.
    state
        .limiter
        .check(
            "github-install-callback",
            &actor.user_id.to_string(),
            INSTALL_CALLBACK_PER_USER,
        )
        .await?;
    let Some(presented) = q.state.as_deref() else {
        let outcome = if q.setup_action.as_deref() == Some("request") {
            "requested"
        } else {
            "updated"
        };
        return Ok(back_to(None, outcome));
    };
    let flow = take_state(&state, "install", presented, &actor).await?;
    // An organization owner has to approve the installation first; nothing is
    // installed yet, so there is nothing to bind.
    if q.setup_action.as_deref() == Some("request") {
        return Ok(back_to(flow.return_to.as_deref(), "requested"));
    }
    let (Some(workspace), Some(app_id)) = (flow.workspace, flow.app) else {
        return Err(anyhow::anyhow!("install state without a workspace or App").into());
    };
    let id = q
        .installation_id
        .filter(|i| *i > 0)
        .ok_or_else(|| AppError::bad_request("missing installation_id"))?;
    // Still an admin of that workspace?
    if !workspace_admin(&state, workspace, actor.user_id).await? {
        return Err(AppError::Forbidden);
    }
    // `installation_id` is just a query parameter: anyone could paste
    // another account's. Only one the returning GitHub user can access binds.
    let not_found = || AppError::bad_request("installation not found");
    let code = q.code.as_deref().ok_or_else(|| {
        AppError::bad_request(
            "GitHub did not send an authorization code; the App needs \"Request user \
             authorization (OAuth) during installation\" turned on",
        )
    })?;
    if !app::user_has_installation(&state, app_id, code, id).await? {
        return Err(not_found());
    }
    let inst = match app::installation(&state, app_id, id).await {
        Err(AppError::BadRequest(_)) => return Err(not_found()),
        r => r?,
    };
    let mut tx = state.db.begin().await?;
    // Re-installing refreshes the account; it never moves to another workspace.
    let bound = sqlx::query(
        "INSERT INTO github_installations (id, app_id, workspace_id, account_login, account_type, created_by)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (id) DO UPDATE
           SET account_login = EXCLUDED.account_login, account_type = EXCLUDED.account_type
           WHERE github_installations.workspace_id = EXCLUDED.workspace_id
             AND github_installations.app_id = EXCLUDED.app_id",
    )
    .bind(id)
    .bind(app_id)
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
    app::forget_lists(&state);
    Ok(back_to(flow.return_to.as_deref(), "installed"))
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct InstallationView {
    id: i64,
    app_id: i64,
    app_slug: String,
    account_login: String,
    account_type: String,
    avatar_url: String,
    /// The installation's settings on GitHub (repository access, suspend).
    configure_url: String,
    #[serde(with = "time::serde::rfc3339")]
    created_at: OffsetDateTime,
}

/// The GitHub accounts and organizations this workspace has installed the App on.
pub async fn installations(
    State(state): State<AppState>,
    grant: Grant,
) -> AppResult<Json<Vec<InstallationView>>> {
    let rows = sqlx::query_as(
        "SELECT g.id, g.app_id, a.slug AS app_slug, g.account_login, g.account_type,
                'https://avatars.githubusercontent.com/' || g.account_login || '?s=64' AS avatar_url,
                CASE WHEN g.account_type = 'Organization'
                     THEN 'https://github.com/organizations/' || g.account_login || '/settings/installations/' || g.id
                     ELSE 'https://github.com/settings/installations/' || g.id END AS configure_url,
                g.created_at
         FROM github_installations g JOIN github_apps a ON a.id = g.app_id
         WHERE g.workspace_id = $1 ORDER BY g.created_at",
    )
    .bind(grant.workspace_id()?)
    .fetch_all(&state.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct ListQuery {
    /// `owner/name`, for branches.
    repo: Option<String>,
    /// Case-insensitive substring of the name.
    #[serde(default)]
    q: String,
    #[serde(default = "default_page")]
    page: usize,
    #[serde(default = "default_per_page", rename = "perPage")]
    per_page: usize,
}

fn default_page() -> usize {
    1
}

fn default_per_page() -> usize {
    50
}

impl ListQuery {
    /// The requested page of the items whose name contains `q`.
    fn apply<T: Clone>(&self, items: &[T], name: impl Fn(&T) -> &str) -> Vec<T> {
        let q = self.q.trim().to_lowercase();
        let per_page = self.per_page.clamp(1, 100);
        items
            .iter()
            .filter(|i| q.is_empty() || name(i).to_lowercase().contains(&q))
            .skip(self.page.max(1).saturating_sub(1).saturating_mul(per_page))
            .take(per_page)
            .cloned()
            .collect()
    }
}

/// Repositories the workspace's installations can reach (the repo picker),
/// most recently pushed first: `?q=` filters, `page`/`perPage` paginate.
pub async fn repos(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<Repo>>> {
    let all = app::workspace_repos(&state, grant.workspace_id()?).await?;
    Ok(Json(q.apply(&all, |r| &r.full_name)))
}

/// Branches of one of those repositories (the branch picker): `?repo=owner/name`.
pub async fn branches(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<ListQuery>,
) -> AppResult<Json<Vec<Branch>>> {
    let repo = q
        .repo
        .as_deref()
        .ok_or_else(|| AppError::bad_request("repo is required"))?;
    let all = app::workspace_branches(&state, grant.workspace_id()?, repo).await?;
    Ok(Json(q.apply(&all, |b| &b.name)))
}

/// GitHub webhook deliveries, authenticated by HMAC under the sending App's
/// secret. GitHub names the App in `X-GitHub-Hook-Installation-Target-ID`;
/// without it every App's secret is tried.
pub async fn webhook(
    State(state): State<AppState>,
    meta: RequestMeta,
    headers: HeaderMap,
    body: Body,
) -> AppResult<(StatusCode, Json<Value>)> {
    // Only deliveries that fail the signature count against the per-IP
    // limit: GitHub sends every App's webhooks from shared addresses, so
    // valid ones (a stranger's installation of this public App included)
    // must not use up the budget of ours.
    let ip = meta.ip_string();
    state
        .limiter
        .peek("github-webhook", &ip, WEBHOOK_PER_IP)
        .await?;
    let rejected = || async {
        match state
            .limiter
            .hit("github-webhook", &ip, WEBHOOK_PER_IP)
            .await
        {
            Err(e) => e,
            Ok(()) => AppError::Unauthorized,
        }
    };
    let header = |k: &str| headers.get(k).and_then(|v| v.to_str().ok());
    let target: Option<i64> =
        header("x-github-hook-installation-target-id").and_then(|v| v.parse().ok());
    let candidates: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT id, webhook_secret_sealed FROM github_apps
         WHERE $1::bigint IS NULL OR id = $1 ORDER BY created_at",
    )
    .bind(target)
    .fetch_all(&state.db)
    .await?;
    let Some(signature) = header("x-hub-signature-256") else {
        return Err(rejected().await);
    };
    if candidates.is_empty() {
        return Err(rejected().await);
    }
    let body = axum::body::to_bytes(body, MAX_WEBHOOK_BODY)
        .await
        .map_err(|_| AppError::bad_request("payload too large"))?;
    let mut sender = None;
    for (id, sealed) in candidates {
        let secret = state
            .keys
            .open(&state.db, &sealed, aad("webhook_secret", &id.to_string()))
            .await?;
        if push::verify_signature(&secret, &body, Some(signature)) {
            sender = Some(id);
            break;
        }
    }
    let Some(app_id) = sender else {
        tracing::warn!("webhook with a bad signature");
        return Err(rejected().await);
    };
    let delivery = header("x-github-delivery")
        .filter(|d| !d.is_empty() && d.len() <= 100)
        .ok_or_else(|| AppError::bad_request("missing X-GitHub-Delivery"))?
        .to_owned();
    let event = header("x-github-event").unwrap_or_default().to_owned();
    // The App is public: anyone can install it and make GitHub send us their
    // events. Those of installations no workspace has bound are dropped
    // before anything is recorded.
    if !bound_installation(&state, app_id, &body).await? {
        return Ok((StatusCode::ACCEPTED, Json(json!({ "ignored": true }))));
    }
    let fresh =
        sqlx::query("INSERT INTO github_deliveries (id) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(&delivery)
            .execute(&state.db)
            .await?
            .rows_affected();
    if fresh == 0 {
        return Ok((StatusCode::OK, Json(json!({ "duplicate": true }))));
    }
    let res = handle_event(&state, &meta, app_id, &delivery, &event, &body).await;
    if matches!(res, Err(AppError::Internal(_))) {
        // Let GitHub's redelivery try again.
        let _ = sqlx::query("DELETE FROM github_deliveries WHERE id = $1")
            .bind(&delivery)
            .execute(&state.db)
            .await;
    }
    res
}

/// Whether the delivery concerns an installation of `app` bound to a
/// workspace (or no installation at all, like `ping`).
async fn bound_installation(state: &AppState, app: i64, body: &[u8]) -> AppResult<bool> {
    #[derive(Deserialize)]
    struct Peek {
        installation: Option<push::InstallationRef>,
    }
    let Ok(Peek {
        installation: Some(i),
    }) = serde_json::from_slice::<Peek>(body)
    else {
        return Ok(true);
    };
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM github_installations WHERE id = $1 AND app_id = $2)",
    )
    .bind(i.id)
    .bind(app)
    .fetch_one(&state.db)
    .await?)
}

/// `app` is the App whose secret signed the delivery: it acts only on that
/// App's installations.
async fn handle_event(
    state: &AppState,
    meta: &RequestMeta,
    app: i64,
    delivery: &str,
    event: &str,
    body: &[u8],
) -> AppResult<(StatusCode, Json<Value>)> {
    let bad = |e: serde_json::Error| AppError::bad_request(format!("invalid {event} payload: {e}"));
    match event {
        "ping" => Ok((StatusCode::OK, Json(json!({})))),
        "push" => {
            let ev: push::PushEvent = serde_json::from_slice(body).map_err(bad)?;
            let ids = push::handle_push(state, meta, app, delivery, &ev).await?;
            Ok((StatusCode::OK, Json(json!({ "deployments": ids }))))
        }
        "pull_request" => {
            let ev: pull_request::PullRequestEvent = serde_json::from_slice(body).map_err(bad)?;
            let out = pull_request::handle_pull_request(state, meta, app, delivery, &ev).await?;
            Ok((StatusCode::OK, Json(out)))
        }
        "check_suite" => {
            let ev: ci::CheckSuiteEvent = serde_json::from_slice(body).map_err(bad)?;
            let ids = ci::handle_check_suite(state, app, &ev).await?;
            Ok((StatusCode::OK, Json(json!({ "settled": ids }))))
        }
        "status" => {
            let ev: ci::StatusEvent = serde_json::from_slice(body).map_err(bad)?;
            let ids = ci::handle_status(state, app, &ev).await?;
            Ok((StatusCode::OK, Json(json!({ "settled": ids }))))
        }
        "installation" | "installation_repositories" => {
            #[derive(Deserialize)]
            struct Ev {
                action: String,
                installation: push::InstallationRef,
            }
            let ev: Ev = serde_json::from_slice(body).map_err(bad)?;
            let id = ev.installation.id;
            // Cached tokens and listings may cover repositories that were just removed.
            app::forget_installation(state, id);
            app::forget_lists(state);
            if event == "installation" && ev.action == "deleted" {
                let mut tx = state.db.begin().await?;
                let ws: Option<Id> = sqlx::query_scalar(
                    "DELETE FROM github_installations WHERE id = $1 AND app_id = $2
                     RETURNING workspace_id",
                )
                .bind(id)
                .bind(app)
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
            } else if event == "installation_repositories" {
                // Repository access changed on GitHub: record it for the workspace.
                let ws: Option<Id> = sqlx::query_scalar(
                    "SELECT workspace_id FROM github_installations WHERE id = $1 AND app_id = $2",
                )
                .bind(id)
                .bind(app)
                .fetch_optional(&state.db)
                .await?;
                if let Some(ws) = ws {
                    Entry::new("github.install.repositories", meta)
                        .workspace(ws)
                        .target("github_installation", id)
                        .metadata(json!({ "action": ev.action }))
                        .record(&state.db)
                        .await?;
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn location(r: Redirect) -> String {
        let resp = r.into_response();
        resp.headers()["location"].to_str().unwrap().to_owned()
    }

    #[test]
    fn app_names_fit_githubs_limit() {
        assert_eq!(app_name("cloud.test", None), "Modula Deploy (cloud.test)");
        assert_eq!(
            app_name("cloud.test", Some("a1b2")),
            "Modula Deploy (cloud.test) a1b2"
        );
        let long = app_name("deploy.some-very-long-company.example", Some("a1b2"));
        assert_eq!(long.chars().count(), MAX_APP_NAME);
        assert!(long.ends_with("…) a1b2"), "{long}");
        assert_eq!(
            app::app_slug("Modula Deploy (cloud.test)"),
            "modula-deploy-cloud-test"
        );
    }

    #[test]
    fn returns_only_to_this_site() {
        assert_eq!(
            location(back_to(Some("/projects/p?env=e&add=repo"), "installed")),
            "/projects/p?env=e&add=repo&github=installed"
        );
        assert_eq!(
            location(back_to(Some("/settings#integrations-github"), "requested")),
            "/settings?github=requested#integrations-github"
        );
        assert_eq!(
            location(back_to(None, "created")),
            "/settings?github=created"
        );
        for bad in [
            "//evil.example/x",
            "https://evil.example",
            "/\\evil.example",
            "x",
        ] {
            assert!(return_path(Some(bad.into())).is_err(), "{bad}");
        }
        assert_eq!(
            return_path(Some("/projects/p".into())).unwrap().as_deref(),
            Some("/projects/p")
        );
    }
}
