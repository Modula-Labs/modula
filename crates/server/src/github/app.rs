//! The instance's GitHub Apps: created through GitHub's manifest flow, one per
//! GitHub account or organization (private: installable only on the account
//! that owns it), and installed per workspace. An App's private key signs
//! short-lived App JWTs, which mint installation tokens scoped to one
//! repository and one permission. Tokens are cached in memory and never logged.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use modula_crypto::envelope::{FieldAad, INSTANCE_SCOPE};
use modula_id::Id;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::Commit;
use crate::deploy::config::{ServiceConfig, validate_repo};
use crate::error::{AppError, AppResult};
use crate::jobs::Job;
use crate::ratelimit::Limit;
use crate::state::AppState;

/// Cached tokens are replaced this long before GitHub expires them.
const TOKEN_MARGIN: Duration = Duration::minutes(5);
const MAX_REPO_PAGES: u32 = 10;

/// Sealed columns are bound to their App (`row` = the App's id).
pub fn aad<'a>(column: &'a str, row: &'a str) -> FieldAad<'a> {
    FieldAad {
        scope: INSTANCE_SCOPE,
        table: "github_apps",
        column,
        row,
    }
}

/// The permissions an App is created with (and that tokens ask for, one
/// [`TokenScope`] at a time).
pub const PERMISSIONS: &[(&str, &str)] = &[
    ("checks", "read"),
    ("contents", "read"),
    ("issues", "write"),
    ("metadata", "read"),
    ("pull_requests", "read"),
    ("statuses", "write"),
];

/// The events an App subscribes to. GitHub always sends Apps their
/// installation and installation_repositories events.
pub const EVENTS: &[&str] = &["check_suite", "pull_request", "push", "status"];

/// What an installation token may do: least privilege per use.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenScope {
    /// Read one repository's contents (branch heads, source tarballs).
    Contents(String),
    /// Post commit statuses on one repository.
    Statuses(String),
    /// Read one repository's check suites and commit statuses (wait for CI).
    Checks(String),
    /// Read one repository's pull requests (changed files).
    PullRequests(String),
    /// Write comments on one repository's pull requests (preview links).
    Issues(String),
    /// List the repositories the installation can access.
    Repos,
}

pub type TokenKey = (i64, TokenScope);

pub struct CachedToken {
    token: SecretString,
    expires_at: OffsetDateTime,
}

struct Creds {
    app_id: i64,
    key: EncodingKey,
}

async fn creds(state: &AppState, app_id: i64) -> AppResult<Creds> {
    let sealed: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT private_key_sealed FROM github_apps WHERE id = $1")
            .bind(app_id)
            .fetch_optional(&state.db)
            .await?;
    let sealed = sealed.ok_or_else(|| {
        AppError::Unavailable("that GitHub App is no longer set up on this instance".into())
    })?;
    let pem = state
        .keys
        .open(&state.db, &sealed, aad("private_key", &app_id.to_string()))
        .await?;
    let key =
        EncodingKey::from_rsa_pem(&pem).map_err(|e| anyhow::anyhow!("GitHub App key: {e}"))?;
    Ok(Creds { app_id, key })
}

/// An App JWT (RS256, 9 minutes; `iat` backdated for clock drift).
fn app_jwt(c: &Creds) -> AppResult<String> {
    #[derive(Serialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: String,
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let claims = Claims {
        iat: now - 60,
        exp: now + 9 * 60,
        iss: c.app_id.to_string(),
    };
    Ok(
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &c.key)
            .map_err(|e| anyhow::anyhow!("signing App JWT: {e}"))?,
    )
}

fn unreachable(e: reqwest::Error) -> AppError {
    tracing::warn!(error = %e, "GitHub API unreachable");
    AppError::Unavailable("GitHub could not be reached; try again".into())
}

fn unexpected(status: u16, what: &str) -> AppError {
    tracing::warn!(status, what, "unexpected GitHub API response");
    AppError::Unavailable("GitHub returned an error; try again".into())
}

fn repo_name(repo: &str) -> &str {
    repo.split_once('/').map_or(repo, |(_, n)| n)
}

pub const HOOK_SYNC_JOB: &str = "github.app_hook_sync";

/// Job: points every App's webhook at this dashboard's current address (it
/// was registered with the address the App was created from; the dashboard
/// may have moved to a domain since). The setup and callback URLs can't be
/// changed through the API; they only matter while installing.
pub async fn hook_sync_job(state: AppState, _job: crate::jobs::Job) -> anyhow::Result<()> {
    let apps: Vec<i64> = sqlx::query_scalar("SELECT id FROM github_apps ORDER BY created_at")
        .fetch_all(&state.db)
        .await?;
    let want = format!(
        "{}{}",
        state.public_origin(),
        crate::api::github::WEBHOOK_PATH
    );
    for app in apps {
        sync_hook(&state, app, &want).await?;
        if let Err(e) = check_permissions(&state, app).await {
            tracing::warn!(app, error = %e, "could not read the GitHub App's permissions");
        }
    }
    Ok(())
}

/// Record which of [`PERMISSIONS`] and [`EVENTS`] the App lacks (`GET /app`
/// as the App): an App created before they were added keeps its old set
/// until its owner approves the new one on GitHub.
pub async fn check_permissions(state: &AppState, app: i64) -> anyhow::Result<()> {
    let jwt = app_jwt(&creds(state, app).await?)?;
    #[derive(Deserialize)]
    struct Resp {
        #[serde(default)]
        permissions: HashMap<String, String>,
        #[serde(default)]
        events: Vec<String>,
    }
    let r: Resp = state
        .github
        .request(Method::GET, "/app")
        .bearer_auth(&jwt)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GitHub unreachable: {e}"))?
        .error_for_status()?
        .json()
        .await?;
    let (missing_permissions, missing_events) = missing(&r.permissions, &r.events);
    sqlx::query(
        "UPDATE github_apps SET missing_permissions = $2, missing_events = $3 WHERE id = $1",
    )
    .bind(app)
    .bind(&missing_permissions)
    .bind(&missing_events)
    .execute(&state.db)
    .await?;
    Ok(())
}

/// The permissions (`name: level`) and events an App with `have` lacks.
/// `write` satisfies `read`.
fn missing(have: &HashMap<String, String>, events: &[String]) -> (Vec<String>, Vec<String>) {
    let perms = PERMISSIONS
        .iter()
        .filter(|(name, level)| {
            !matches!(
                (have.get(*name).map(String::as_str), *level),
                (Some("write" | "admin"), _) | (Some("read"), "read")
            )
        })
        .map(|(name, level)| format!("{name}: {level}"))
        .collect();
    let events = EVENTS
        .iter()
        .filter(|e| !events.iter().any(|h| h == *e))
        .map(|e| e.to_string())
        .collect();
    (perms, events)
}

async fn sync_hook(state: &AppState, app: i64, want: &str) -> anyhow::Result<()> {
    let jwt = app_jwt(&creds(state, app).await?)?;
    let resp = state
        .github
        .request(Method::GET, "/app/hook/config")
        .bearer_auth(&jwt)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GitHub unreachable: {e}"))?;
    #[derive(Deserialize)]
    struct HookConfig {
        url: Option<String>,
    }
    let current: HookConfig = resp.error_for_status()?.json().await?;
    if current.url.as_deref() == Some(want) {
        return Ok(());
    }
    state
        .github
        .request(Method::PATCH, "/app/hook/config")
        .bearer_auth(&jwt)
        .json(&json!({ "url": want }))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GitHub unreachable: {e}"))?
        .error_for_status()?;
    tracing::info!(app, url = %want, previous = ?current.url, "GitHub App webhook URL updated");
    Ok(())
}

/// Queue [`hook_sync_job`] (startup, and after the dashboard's domain changes).
pub async fn queue_hook_sync(db: &sqlx::PgPool) -> AppResult<()> {
    crate::jobs::enqueue(
        db,
        crate::jobs::NewJob {
            kind: HOOK_SYNC_JOB,
            payload: json!({}),
            dedupe_key: Some(HOOK_SYNC_JOB),
            ..Default::default()
        },
    )
    .await?;
    Ok(())
}

/// A token for `installation`, from the cache or freshly minted.
pub async fn installation_token(
    state: &AppState,
    installation: i64,
    scope: TokenScope,
) -> AppResult<SecretString> {
    let key = (installation, scope);
    let now = OffsetDateTime::now_utc();
    if let Some(t) = crate::sync::lock(&state.github.tokens).get(&key)
        && t.expires_at - TOKEN_MARGIN > now
    {
        return Ok(t.token.clone());
    }
    let body = match &key.1 {
        TokenScope::Contents(repo) => {
            json!({ "repositories": [repo_name(repo)], "permissions": { "contents": "read" } })
        }
        TokenScope::Statuses(repo) => {
            json!({ "repositories": [repo_name(repo)], "permissions": { "statuses": "write" } })
        }
        TokenScope::Checks(repo) => json!({
            "repositories": [repo_name(repo)],
            "permissions": { "checks": "read", "statuses": "read" },
        }),
        TokenScope::PullRequests(repo) => {
            json!({ "repositories": [repo_name(repo)], "permissions": { "pull_requests": "read" } })
        }
        TokenScope::Issues(repo) => {
            json!({ "repositories": [repo_name(repo)], "permissions": { "issues": "write" } })
        }
        TokenScope::Repos => json!({ "permissions": { "metadata": "read" } }),
    };
    let app: Option<i64> =
        sqlx::query_scalar("SELECT app_id FROM github_installations WHERE id = $1")
            .bind(installation)
            .fetch_optional(&state.db)
            .await?;
    let app = app.ok_or_else(|| AppError::bad_request("unknown GitHub App installation"))?;
    let jwt = app_jwt(&creds(state, app).await?)?;
    let resp = state
        .github
        .request(
            Method::POST,
            &format!("/app/installations/{installation}/access_tokens"),
        )
        .bearer_auth(jwt)
        .json(&body)
        .send()
        .await
        .map_err(unreachable)?;
    match resp.status().as_u16() {
        201 => {}
        404 | 422 => {
            return Err(AppError::bad_request(
                "the GitHub App installation cannot access this repository",
            ));
        }
        s => return Err(unexpected(s, "access token")),
    }
    #[derive(Deserialize)]
    struct Resp {
        token: SecretString,
        #[serde(with = "time::serde::rfc3339")]
        expires_at: OffsetDateTime,
    }
    let r: Resp = resp.json().await.map_err(anyhow::Error::from)?;
    let mut cache = crate::sync::lock(&state.github.tokens);
    cache.retain(|_, t| t.expires_at - TOKEN_MARGIN > now);
    cache.insert(
        key,
        CachedToken {
            token: r.token.clone(),
            expires_at: r.expires_at,
        },
    );
    Ok(r.token)
}

/// Forget an installation's cached tokens (it was uninstalled).
pub fn forget_installation(state: &AppState, installation: i64) {
    crate::sync::lock(&state.github.tokens).retain(|(id, _), _| *id != installation);
}

/// Forget every cached token (an App was removed).
pub fn forget_tokens(state: &AppState) {
    crate::sync::lock(&state.github.tokens).clear();
}

/// The installation in `workspace` that covers `repo`'s owner, if any.
pub async fn installation_for(
    state: &AppState,
    workspace: Id,
    repo: &str,
) -> AppResult<Option<i64>> {
    let owner = repo.split_once('/').map_or(repo, |(o, _)| o);
    Ok(sqlx::query_scalar(
        "SELECT id FROM github_installations
         WHERE workspace_id = $1 AND lower(account_login) = lower($2)
         ORDER BY created_at LIMIT 1",
    )
    .bind(workspace)
    .bind(owner)
    .fetch_optional(&state.db)
    .await?)
}

/// A token for `repo` if the workspace has an installation for its owner.
/// GitHub-side failures fall back to `None` (public access), so a public
/// repository still deploys while GitHub's token endpoint is unhappy.
pub async fn repo_token(
    state: &AppState,
    workspace: Id,
    repo: &str,
    scope: TokenScope,
) -> AppResult<Option<SecretString>> {
    let Some(installation) = installation_for(state, workspace, repo).await? else {
        return Ok(None);
    };
    match installation_token(state, installation, scope).await {
        Ok(t) => Ok(Some(t)),
        Err(AppError::Internal(e)) => Err(AppError::Internal(e)),
        Err(e) => {
            tracing::warn!(error = %e, installation, "no installation token; trying without");
            Ok(None)
        }
    }
}

/// The head of `branch`, authenticated through the workspace's installation
/// when there is one (private repositories).
pub async fn branch_head(
    state: &AppState,
    workspace: Id,
    repo: &str,
    branch: &str,
) -> AppResult<Commit> {
    let token = repo_token(
        state,
        workspace,
        repo,
        TokenScope::Contents(repo.to_owned()),
    )
    .await?;
    state
        .github
        .branch_head(repo, branch, token.as_ref().map(|t| t.expose_secret()))
        .await
}

/// `Authorization` value the node sends when downloading the source tarball
/// (empty for public repositories). Never fails the snapshot: a GitHub hiccup
/// fails only this build.
pub async fn source_auth(state: &AppState, workspace: Id, repo: &str) -> AppResult<String> {
    // Cheap check first: most snapshots have no installation to consult.
    if installation_for(state, workspace, repo).await?.is_none() {
        return Ok(String::new());
    }
    Ok(
        match repo_token(
            state,
            workspace,
            repo,
            TokenScope::Contents(repo.to_owned()),
        )
        .await
        {
            Ok(Some(t)) => format!("Bearer {}", t.expose_secret()),
            Ok(None) => String::new(),
            Err(e) => {
                tracing::warn!(error = %e, "could not mint a source token");
                String::new()
            }
        },
    )
}

/// What GitHub returns when a manifest is converted into an App.
#[derive(Deserialize)]
pub struct Conversion {
    pub id: i64,
    pub slug: String,
    pub client_id: String,
    pub client_secret: SecretString,
    pub webhook_secret: Option<SecretString>,
    pub pem: SecretString,
    pub html_url: String,
    pub owner: Option<Owner>,
}

#[derive(Deserialize)]
pub struct Owner {
    pub login: String,
}

/// Exchange the one-time manifest `code` for the new App's credentials.
pub async fn convert_manifest(state: &AppState, code: &str) -> AppResult<Conversion> {
    let ok = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    if code.is_empty() || code.len() > 100 || !code.bytes().all(ok) {
        return Err(AppError::bad_request("invalid code"));
    }
    let resp = state
        .github
        .request(Method::POST, &format!("/app-manifests/{code}/conversions"))
        .send()
        .await
        .map_err(unreachable)?;
    match resp.status().as_u16() {
        201 => Ok(resp.json().await.map_err(anyhow::Error::from)?),
        404 | 422 => Err(AppError::bad_request(
            "GitHub rejected the code; create the App again",
        )),
        s => Err(unexpected(s, "manifest conversion")),
    }
}

/// GitHub's slug for an App name: lowercased, runs of other characters
/// turned into single hyphens ("Modula Deploy (a.io)" → "modula-deploy-a-io").
pub fn app_slug(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// Whether a public App already has this name's slug (App names are global
/// on GitHub). Best effort: if GitHub can't tell us, assume it's free and let
/// GitHub's own form report a clash.
pub async fn app_name_taken(state: &AppState, name: &str) -> bool {
    let slug = app_slug(name);
    if slug.is_empty() {
        return false;
    }
    match state
        .github
        .request(Method::GET, &format!("/apps/{slug}"))
        .send()
        .await
    {
        Ok(resp) => resp.status().as_u16() == 200,
        Err(e) => {
            tracing::debug!(error = %e, "could not check the GitHub App name");
            false
        }
    }
}

pub struct Installation {
    pub account_login: String,
    pub account_type: String,
}

/// Look an installation up as `app` (proves it belongs to that App).
pub async fn installation(state: &AppState, app: i64, id: i64) -> AppResult<Installation> {
    let jwt = app_jwt(&creds(state, app).await?)?;
    let resp = state
        .github
        .request(Method::GET, &format!("/app/installations/{id}"))
        .bearer_auth(jwt)
        .send()
        .await
        .map_err(unreachable)?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Err(AppError::bad_request("unknown GitHub App installation")),
        s => return Err(unexpected(s, "installation")),
    }
    #[derive(Deserialize)]
    struct Resp {
        account: Account,
    }
    #[derive(Deserialize)]
    struct Account {
        login: String,
        #[serde(rename = "type")]
        kind: String,
    }
    let r: Resp = resp.json().await.map_err(anyhow::Error::from)?;
    Ok(Installation {
        account_login: r.account.login,
        account_type: r.account.kind,
    })
}

/// Where GitHub's web UI (and OAuth) lives for the configured API: github.com
/// for api.github.com, the same host for GitHub Enterprise Server
/// (`https://ghe.example/api/v3` → `https://ghe.example`) and test mocks.
pub fn web_origin(state: &AppState) -> url::Url {
    let api = &state.github.api;
    if api.host_str() == Some("api.github.com") {
        return "https://github.com".parse().expect("static URL");
    }
    let mut web = api.clone();
    web.set_path("");
    web.set_query(None);
    web
}

/// Whether `raw` is an https URL on GitHub's web host (github.com or the
/// configured Enterprise host), so it is safe to send a browser to.
pub fn is_github_web_url(state: &AppState, raw: &str) -> bool {
    let Ok(url) = raw.parse::<url::Url>() else {
        return false;
    };
    let web = web_origin(state);
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && url
            .host_str()
            .is_some_and(|h| h == "github.com" || Some(h) == web.host_str())
}

/// Whether the GitHub user who just authorized the App (`code`, from the
/// installation's OAuth step) can access `installation`: the callback's
/// `installation_id` is attacker-controlled, the user's own list isn't.
/// The user token is used for this one check and dropped.
pub async fn user_has_installation(
    state: &AppState,
    app: i64,
    code: &str,
    installation: i64,
) -> AppResult<bool> {
    let ok = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    if code.is_empty() || code.len() > 100 || !code.bytes().all(ok) {
        return Err(AppError::bad_request("invalid code"));
    }
    let row: Option<(String, Vec<u8>)> =
        sqlx::query_as("SELECT client_id, client_secret_sealed FROM github_apps WHERE id = $1")
            .bind(app)
            .fetch_optional(&state.db)
            .await?;
    let Some((client_id, sealed)) = row else {
        return Err(AppError::Unavailable(
            "that GitHub App is no longer set up on this instance".into(),
        ));
    };
    let secret = state
        .keys
        .open(&state.db, &sealed, aad("client_secret", &app.to_string()))
        .await?;
    let secret =
        zeroize::Zeroizing::new(String::from_utf8(secret.to_vec()).map_err(anyhow::Error::from)?);
    let mut url = web_origin(state);
    url.set_path("/login/oauth/access_token");
    let resp = state
        .github
        .http
        .post(url)
        .header("Accept", "application/json")
        .json(&json!({ "client_id": client_id, "client_secret": secret.as_str(), "code": code }))
        .send()
        .await
        .map_err(unreachable)?;
    if !resp.status().is_success() {
        return Err(unexpected(resp.status().as_u16(), "OAuth code exchange"));
    }
    // GitHub answers 200 with `error` for a bad or expired code.
    #[derive(Deserialize)]
    struct Exchange {
        access_token: Option<SecretString>,
    }
    let ex: Exchange = resp.json().await.map_err(anyhow::Error::from)?;
    let token = ex.access_token.ok_or_else(|| {
        AppError::bad_request("GitHub rejected the authorization; connect GitHub again")
    })?;
    #[derive(Deserialize)]
    struct Page {
        installations: Vec<Inst>,
    }
    #[derive(Deserialize)]
    struct Inst {
        id: i64,
    }
    for page in 1..=MAX_REPO_PAGES {
        let resp = state
            .github
            .request(
                Method::GET,
                &format!("/user/installations?per_page=100&page={page}"),
            )
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(unreachable)?;
        match resp.status().as_u16() {
            200 => {}
            s => return Err(unexpected(s, "user installations")),
        }
        let p: Page = resp.json().await.map_err(anyhow::Error::from)?;
        if p.installations.iter().any(|i| i.id == installation) {
            return Ok(true);
        }
        if p.installations.len() < 100 {
            break;
        }
    }
    Ok(false)
}

/// How long the pickers' repository and branch listings are reused.
const LIST_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// GitHub calls the pickers may make per workspace (cache misses only).
const LIST_RATE: Limit = Limit::per_minute(30);

/// A listing and when it was fetched.
type Cached<T> = (Instant, Arc<Vec<T>>);

/// Recent listings per workspace (see `Client::lists`).
#[derive(Default)]
pub struct ListCache {
    repos: HashMap<Id, Cached<Repo>>,
    /// Keyed by workspace and lowercased `owner/name`.
    branches: HashMap<(Id, String), Cached<Branch>>,
}

/// Drop every cached listing: installations or their repositories changed.
pub fn forget_lists(state: &AppState) {
    *crate::sync::lock(&state.github.lists) = ListCache::default();
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
    /// When something was last pushed (GitHub's `pushed_at`).
    #[serde(with = "time::serde::rfc3339::option")]
    pub updated_at: Option<OffsetDateTime>,
    pub owner_avatar: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    pub name: String,
    #[serde(default)]
    pub protected: bool,
}

/// Only GitHub's avatar host is allowed by the dashboard's CSP.
fn avatar(url: Option<String>) -> Option<String> {
    url.filter(|u| u.starts_with("https://avatars.githubusercontent.com/"))
}

/// Repositories reachable through every installation of `workspace`, most
/// recently pushed first. Cached for a minute; an installation GitHub won't
/// mint a token for (suspended, removed) is skipped rather than failing all.
pub async fn workspace_repos(state: &AppState, workspace: Id) -> AppResult<Arc<Vec<Repo>>> {
    if let Some((at, repos)) = crate::sync::lock(&state.github.lists).repos.get(&workspace)
        && at.elapsed() < LIST_TTL
    {
        return Ok(repos.clone());
    }
    let installations: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM github_installations WHERE workspace_id = $1 ORDER BY created_at",
    )
    .bind(workspace)
    .fetch_all(&state.db)
    .await?;
    if installations.is_empty() {
        return Ok(Arc::default());
    }
    state
        .limiter
        .check("github_lists", &workspace.to_string(), LIST_RATE)
        .await?;
    #[derive(Deserialize)]
    struct Page {
        repositories: Vec<GhRepo>,
    }
    #[derive(Deserialize)]
    struct GhRepo {
        full_name: String,
        private: bool,
        default_branch: String,
        #[serde(default, with = "time::serde::rfc3339::option")]
        pushed_at: Option<OffsetDateTime>,
        #[serde(default, with = "time::serde::rfc3339::option")]
        updated_at: Option<OffsetDateTime>,
        owner: Option<GhOwner>,
    }
    #[derive(Deserialize)]
    struct GhOwner {
        avatar_url: Option<String>,
    }
    let mut out: Vec<Repo> = Vec::new();
    let mut last_err = None;
    let mut reached = 0;
    for id in installations {
        let token = match installation_token(state, id, TokenScope::Repos).await {
            Ok(t) => t,
            Err(e @ AppError::Internal(_)) => return Err(e),
            Err(e) => {
                tracing::warn!(error = %e, installation = id, "skipping installation in the repo list");
                last_err = Some(e);
                continue;
            }
        };
        reached += 1;
        for page in 1..=MAX_REPO_PAGES {
            let resp = state
                .github
                .request(
                    Method::GET,
                    &format!("/installation/repositories?per_page=100&page={page}"),
                )
                .bearer_auth(token.expose_secret())
                .send()
                .await
                .map_err(unreachable)?;
            if resp.status().as_u16() != 200 {
                return Err(unexpected(
                    resp.status().as_u16(),
                    "installation repositories",
                ));
            }
            let p: Page = resp.json().await.map_err(anyhow::Error::from)?;
            let n = p.repositories.len();
            out.extend(p.repositories.into_iter().map(|r| Repo {
                full_name: r.full_name,
                private: r.private,
                default_branch: r.default_branch,
                updated_at: r.pushed_at.or(r.updated_at),
                owner_avatar: avatar(r.owner.and_then(|o| o.avatar_url)),
            }));
            if n < 100 {
                break;
            }
        }
    }
    if reached == 0
        && let Some(e) = last_err
    {
        return Err(e);
    }
    let mut seen = std::collections::HashSet::new();
    out.retain(|r| seen.insert(r.full_name.to_lowercase()));
    // Most recently pushed first, then by name.
    out.sort_by(|a, b| {
        b.updated_at
            .cmp(&a.updated_at)
            .then_with(|| a.full_name.to_lowercase().cmp(&b.full_name.to_lowercase()))
    });
    let out = Arc::new(out);
    crate::sync::lock(&state.github.lists)
        .repos
        .insert(workspace, (Instant::now(), out.clone()));
    Ok(out)
}

/// Branches of `repo`, which must be reachable through one of the
/// workspace's installations (a repository of another workspace's, or one no
/// installation covers, is `NotFound`). Cached for a minute.
pub async fn workspace_branches(
    state: &AppState,
    workspace: Id,
    repo: &str,
) -> AppResult<Arc<Vec<Branch>>> {
    validate_repo(repo)?;
    let key = (workspace, repo.to_lowercase());
    if let Some((at, branches)) = crate::sync::lock(&state.github.lists).branches.get(&key)
        && at.elapsed() < LIST_TTL
    {
        return Ok(branches.clone());
    }
    let Some(installation) = installation_for(state, workspace, repo).await? else {
        return Err(AppError::NotFound);
    };
    state
        .limiter
        .check("github_lists", &workspace.to_string(), LIST_RATE)
        .await?;
    // Scoped to this one repository: GitHub refuses (422) when the
    // installation can't reach it.
    let token = installation_token(state, installation, TokenScope::Contents(repo.to_owned()))
        .await
        .map_err(|e| match e {
            AppError::BadRequest(_) => AppError::NotFound,
            e => e,
        })?;
    let mut out = Vec::new();
    for page in 1..=MAX_REPO_PAGES {
        let resp = state
            .github
            .request(
                Method::GET,
                &format!("/repos/{repo}/branches?per_page=100&page={page}"),
            )
            .bearer_auth(token.expose_secret())
            .send()
            .await
            .map_err(unreachable)?;
        match resp.status().as_u16() {
            200 => {}
            404 => return Err(AppError::NotFound),
            s => return Err(unexpected(s, "branches")),
        }
        let p: Vec<Branch> = resp.json().await.map_err(anyhow::Error::from)?;
        let n = p.len();
        out.extend(p);
        if n < 100 {
            break;
        }
    }
    let out = Arc::new(out);
    let mut cache = crate::sync::lock(&state.github.lists);
    cache.branches.retain(|_, (at, _)| at.elapsed() < LIST_TTL);
    cache.branches.insert(key, (Instant::now(), out.clone()));
    Ok(out)
}

/// One entry of a repository's git tree.
#[derive(Debug, Clone, Deserialize)]
pub struct TreeEntry {
    pub path: String,
    /// `blob`, `tree` or `commit` (a submodule).
    #[serde(rename = "type")]
    pub kind: String,
    pub sha: String,
    /// Blobs only.
    #[serde(default)]
    pub size: Option<u64>,
}

/// Every path in the tree at commit `sha`, or None when the listing is over
/// `max_bytes` (GitHub itself truncates huge trees; what it returns is used).
pub async fn repo_tree(
    state: &AppState,
    token: &SecretString,
    repo: &str,
    sha: &str,
    max_bytes: usize,
) -> AppResult<Option<Vec<TreeEntry>>> {
    if !super::push::is_sha(sha) {
        return Err(AppError::bad_request("invalid commit"));
    }
    let resp = state
        .github
        .request(
            Method::GET,
            &format!("/repos/{repo}/git/trees/{sha}?recursive=1"),
        )
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .map_err(unreachable)?;
    match resp.status().as_u16() {
        200 => {}
        // 409: an empty repository.
        404 | 409 => return Ok(None),
        s => return Err(unexpected(s, "tree")),
    }
    #[derive(Deserialize)]
    struct Resp {
        tree: Vec<TreeEntry>,
    }
    let Some(body) = read_capped(resp, max_bytes).await? else {
        return Ok(None);
    };
    let r: Resp = serde_json::from_slice(&body).map_err(anyhow::Error::from)?;
    Ok(Some(r.tree))
}

/// A blob's raw bytes, or None when it is over `max_bytes` or gone.
pub async fn repo_blob(
    state: &AppState,
    token: &SecretString,
    repo: &str,
    sha: &str,
    max_bytes: usize,
) -> AppResult<Option<Vec<u8>>> {
    if !super::push::is_sha(sha) {
        return Err(AppError::bad_request("invalid blob"));
    }
    // Not `request()`: that asks for JSON, and headers append.
    let resp = state
        .github
        .http
        .get(state.github.url(&format!("/repos/{repo}/git/blobs/{sha}")))
        .header("Accept", "application/vnd.github.raw+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(token.expose_secret())
        .send()
        .await
        .map_err(unreachable)?;
    match resp.status().as_u16() {
        200 => {}
        404 => return Ok(None),
        s => return Err(unexpected(s, "blob")),
    }
    read_capped(resp, max_bytes).await
}

/// The body, unless it grows past `max` bytes.
async fn read_capped(mut resp: reqwest::Response, max: usize) -> AppResult<Option<Vec<u8>>> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Ok(None);
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(unreachable)? {
        if out.len() + chunk.len() > max {
            return Ok(None);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(Some(out))
}

pub const STATUS_JOB: &str = "github.commit_status";

/// Job: report a deployment's state on its commit (`pending`, `success`,
/// `failure`). Deployments without an installation for their repository are
/// skipped; GitHub errors fail the job so the queue retries it.
pub async fn status_job(state: AppState, job: Job) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        deployment_id: Id,
        state: String,
    }
    let p: Payload = serde_json::from_value(job.payload)?;
    let description = match p.state.as_str() {
        "pending" => "Building",
        "success" => "Deployed",
        "failure" => "Deployment failed",
        other => anyhow::bail!("unknown commit status {other}"),
    };
    #[derive(sqlx::FromRow)]
    struct Row {
        commit_sha: Option<String>,
        config: serde_json::Value,
        service: String,
        service_id: Id,
        project: Id,
        env: Id,
        workspace: Id,
    }
    let row: Option<Row> = sqlx::query_as(
        "SELECT d.commit_sha, d.config, s.name AS service, s.id AS service_id, p.id AS project,
                i.environment_id AS env, p.workspace_id AS workspace
         FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         WHERE d.id = $1",
    )
    .bind(p.deployment_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(Row {
        commit_sha: Some(sha),
        config,
        service,
        service_id,
        project,
        env,
        workspace,
    }) = row
    else {
        return Ok(());
    };
    let cfg: ServiceConfig = serde_json::from_value(config)?;
    let Some((repo, _, _)) = cfg.repo() else {
        return Ok(());
    };
    let Some(installation) = installation_for(&state, workspace, repo).await? else {
        return Ok(());
    };
    let token = installation_token(&state, installation, TokenScope::Statuses(repo.to_owned()))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut target = state.public_url();
    target.set_path(&format!("/projects/{project}"));
    target
        .query_pairs_mut()
        .append_pair("env", &env.to_string())
        .append_pair("service", &service_id.to_string())
        .append_pair("tab", "deployments")
        .append_pair("deployment", &p.deployment_id.to_string());
    let resp = state
        .github
        .request(Method::POST, &format!("/repos/{repo}/statuses/{sha}"))
        .bearer_auth(token.expose_secret())
        .json(&json!({
            "state": p.state,
            "target_url": target.as_str(),
            "description": description,
            "context": format!("modula/{service}"),
        }))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("GitHub answered {status} to a commit status");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_permissions_and_events() {
        let have: HashMap<String, String> = [
            ("contents", "read"),
            ("metadata", "read"),
            ("statuses", "write"),
            ("checks", "write"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let (p, e) = missing(&have, &["push".to_owned()]);
        assert_eq!(p, ["issues: write", "pull_requests: read"]);
        assert_eq!(e, ["check_suite", "pull_request", "status"]);
        let all: HashMap<String, String> = PERMISSIONS
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let events: Vec<String> = EVENTS.iter().map(|e| e.to_string()).collect();
        assert_eq!(missing(&all, &events), (vec![], vec![]));
    }
}
