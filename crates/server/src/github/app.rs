//! The instance's GitHub App: created once through GitHub's manifest flow,
//! installed per workspace. Its private key signs short-lived App JWTs, which
//! mint installation tokens scoped to one repository and one permission.
//! Tokens are cached in memory and never logged.

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use modula_crypto::envelope::{FieldAad, INSTANCE_SCOPE};
use modula_id::Id;
use reqwest::Method;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::{Duration, OffsetDateTime};

use super::Commit;
use crate::deploy::config::ServiceConfig;
use crate::error::{AppError, AppResult};
use crate::jobs::Job;
use crate::state::AppState;

/// Cached tokens are replaced this long before GitHub expires them.
const TOKEN_MARGIN: Duration = Duration::minutes(5);
const MAX_REPO_PAGES: u32 = 10;

pub fn aad(column: &str) -> FieldAad<'_> {
    FieldAad {
        scope: INSTANCE_SCOPE,
        table: "github_app",
        column,
        row: "app",
    }
}

/// What an installation token may do: least privilege per use.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenScope {
    /// Read one repository's contents (branch heads, source tarballs).
    Contents(String),
    /// Post commit statuses on one repository.
    Statuses(String),
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

async fn creds(state: &AppState) -> AppResult<Creds> {
    let row: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT app_id, private_key_sealed FROM github_app")
            .fetch_optional(&state.db)
            .await?;
    let (app_id, sealed) = row.ok_or_else(|| {
        AppError::Unavailable("the GitHub App has not been set up on this instance".into())
    })?;
    let pem = state
        .keys
        .open(&state.db, &sealed, aad("private_key"))
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

/// A token for `installation`, from the cache or freshly minted.
pub async fn installation_token(
    state: &AppState,
    installation: i64,
    scope: TokenScope,
) -> AppResult<SecretString> {
    let key = (installation, scope);
    let now = OffsetDateTime::now_utc();
    if let Some(t) = state.github.tokens.lock().expect("token cache").get(&key)
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
        TokenScope::Repos => json!({ "permissions": { "metadata": "read" } }),
    };
    let jwt = app_jwt(&creds(state).await?)?;
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
    let mut cache = state.github.tokens.lock().expect("token cache");
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
    state
        .github
        .tokens
        .lock()
        .expect("token cache")
        .retain(|(id, _), _| *id != installation);
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

pub struct Installation {
    pub account_login: String,
    pub account_type: String,
}

/// Look an installation up as the App (proves it belongs to our App).
pub async fn installation(state: &AppState, id: i64) -> AppResult<Installation> {
    let jwt = app_jwt(&creds(state).await?)?;
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

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repo {
    pub full_name: String,
    pub private: bool,
    pub default_branch: String,
}

/// Repositories reachable through every installation of `workspace`.
pub async fn workspace_repos(state: &AppState, workspace: Id) -> AppResult<Vec<Repo>> {
    let installations: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM github_installations WHERE workspace_id = $1 ORDER BY created_at",
    )
    .bind(workspace)
    .fetch_all(&state.db)
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
    }
    let mut out = Vec::new();
    for id in installations {
        let token = installation_token(state, id, TokenScope::Repos).await?;
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
            }));
            if n < 100 {
                break;
            }
        }
    }
    out.sort_by_key(|r| r.full_name.to_lowercase());
    out.dedup_by(|a, b| a.full_name.eq_ignore_ascii_case(&b.full_name));
    Ok(out)
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
    let mut target = state.cfg.public_url.clone();
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
