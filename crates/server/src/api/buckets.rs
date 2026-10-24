//! Cloudflare R2 buckets on the project canvas.
//!
//! An R2 bucket is a service (template `r2`) whose config source is
//! [`Source::R2`]: nothing runs or deploys. Each environment's instance gets
//! its own bucket, created or linked through the workspace's Cloudflare
//! connection, plus an account-owned API token scoped to that bucket alone.
//! The token's S3 credentials become the instance's variables (the secret is
//! sealed), so other services use `${{media.S3_BUCKET}}` and friends. An
//! instance without an `r2_buckets` row has no bucket yet: environments added
//! later start that way, and the dashboard offers to create or link one.
//!
//! Self-hosted buckets are an ordinary template (`templates::CATALOG`).
//!
//! [`Source::R2`]: crate::deploy::config::Source::R2

use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::State;
use modula_id::Id;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::activity::{self, Change};
use crate::api::services::{CreatedService, load_instance, validate_service_name};
use crate::api::variables::{self, UpdateVariables};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::deploy::config::ServiceConfig;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::integrations::cloudflare::{self, Connection, client, r2};
use crate::state::AppState;

/// `services.template` of an R2 bucket.
pub const R2_TEMPLATE: &str = "r2";

/// The variables an R2 instance gets; all removed when its bucket goes.
pub const VARIABLES: [&str; 7] = [
    "S3_BUCKET",
    "BUCKET_NAME",
    "S3_ENDPOINT",
    "S3_REGION",
    "S3_FORCE_PATH_STYLE",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
];

pub fn r2_config() -> ServiceConfig {
    serde_json::from_value(json!({ "source": { "type": "r2" } })).expect("static config")
}

/// Which bucket an instance gets: a new one by that name, or an existing one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Choice {
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub existing: Option<String>,
}

enum Pick {
    New(String),
    Existing(String),
}

impl Choice {
    fn pick(self) -> AppResult<Pick> {
        let clean = |s: Option<String>| s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        let pick = match (clean(self.bucket), clean(self.existing)) {
            (Some(b), None) => Pick::New(b),
            (None, Some(b)) => Pick::Existing(b),
            _ => {
                return Err(AppError::bad_request(
                    "give either a new bucket's name or an existing bucket to link",
                ));
            }
        };
        let (Pick::New(name) | Pick::Existing(name)) = &pick;
        if !r2::valid_bucket_name(name) {
            return Err(AppError::bad_request(
                "R2 bucket names are 3–63 lowercase letters, digits and hyphens, starting and ending with a letter or digit",
            ));
        }
        Ok(pick)
    }
}

async fn require_connection(state: &AppState, workspace: Id) -> AppResult<(Connection, String)> {
    let conn = cloudflare::connection(state, workspace)
        .await?
        .ok_or_else(|| {
            AppError::Conflict(
                "connect Cloudflare in Settings → Integrations to use R2 buckets".into(),
            )
        })?;
    let account = conn.metadata.account_id.clone().ok_or_else(|| {
        AppError::Conflict(
            "the Cloudflare token can't see an account; reconnect it with one".into(),
        )
    })?;
    Ok((conn, account))
}

/// A failed R2 bucket call, saying which permission is missing when that's why.
fn r2_error(e: client::Error) -> AppError {
    if e.is_forbidden() {
        AppError::bad_request(format!(
            "the Cloudflare token can't manage R2 buckets: add the {} permission to it (Settings → Integrations → Cloudflare)",
            r2::R2_PERMISSION
        ))
    } else {
        cloudflare::upstream(e)
    }
}

/// A failed token call, likewise.
fn token_error(e: client::Error) -> AppError {
    if e.is_forbidden() {
        AppError::bad_request(format!(
            "the Cloudflare token can't create API tokens for the bucket: add the {} permission to it (Settings → Integrations → Cloudflare)",
            r2::TOKENS_PERMISSION
        ))
    } else {
        cloudflare::upstream(e)
    }
}

/// What a provisioned bucket left at Cloudflare, to undo on a later failure.
struct Made {
    account: String,
    bucket: String,
    created: bool,
    token_id: String,
}

impl Made {
    async fn undo(&self, conn: &Connection) {
        if let Err(e) = conn
            .client
            .delete_account_token(&self.account, &self.token_id)
            .await
        {
            tracing::warn!(error = %e, "could not revoke an unused R2 bucket token");
        }
        if self.created
            && let Err(e) = conn
                .client
                .delete_r2_bucket(&self.account, &self.bucket)
                .await
        {
            tracing::warn!(error = %e, bucket = %self.bucket, "could not delete an unused R2 bucket");
        }
    }
}

/// Gives an R2 instance its bucket and credentials (see the module docs).
async fn provision(
    state: &AppState,
    actor: &Actor,
    grant: &Grant,
    meta: &RequestMeta,
    instance: Id,
    pick: Pick,
) -> AppResult<()> {
    let workspace = grant.workspace_id()?;
    let (template, service, environment): (Option<String>, String, String) = sqlx::query_as(
        "SELECT s.template, s.name, e.name FROM service_instances i
         JOIN services s ON s.id = i.service_id JOIN environments e ON e.id = i.environment_id
         WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?
    .ok_or(AppError::NotFound)?;
    if template.as_deref() != Some(R2_TEMPLATE) {
        return Err(AppError::bad_request("this service is not an R2 bucket"));
    }
    let has: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM r2_buckets WHERE service_instance_id = $1)",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    if has {
        return Err(AppError::Conflict(
            "this environment already has a bucket".into(),
        ));
    }
    let (conn, account) = require_connection(state, workspace).await?;

    let (bucket, created) = match pick {
        Pick::Existing(name) => {
            let buckets = conn.client.r2_buckets(&account).await.map_err(r2_error)?;
            if !buckets.iter().any(|b| b.name == name) {
                return Err(AppError::bad_request(format!(
                    "there is no bucket named {name} in the Cloudflare account"
                )));
            }
            (name, false)
        }
        Pick::New(name) => match conn.client.create_r2_bucket(&account, &name).await {
            Ok(client::Change::Unchanged) => {
                return Err(AppError::Conflict(format!(
                    "a bucket named {name} already exists in the Cloudflare account: link it instead"
                )));
            }
            Ok(_) => (name, true),
            Err(e) => return Err(r2_error(e)),
        },
    };
    let token_name = format!("Modula Deploy: {bucket} ({service}, {environment})");
    let creds = match conn
        .client
        .create_bucket_token(&account, &bucket, &token_name)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            if created && let Err(e) = conn.client.delete_r2_bucket(&account, &bucket).await {
                tracing::warn!(error = %e, %bucket, "could not delete an unused R2 bucket");
            }
            return Err(token_error(e));
        }
    };
    let made = Made {
        account: account.clone(),
        bucket: bucket.clone(),
        created,
        token_id: creds.token_id.clone(),
    };

    let stored = sqlx::query(
        "INSERT INTO r2_buckets (service_instance_id, account_id, bucket, token_id, created)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (service_instance_id) DO NOTHING",
    )
    .bind(instance)
    .bind(&account)
    .bind(&bucket)
    .bind(&creds.token_id)
    .bind(created)
    .execute(&state.db)
    .await;
    match stored {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => {
            made.undo(&conn).await;
            return Err(AppError::Conflict(
                "this environment already has a bucket".into(),
            ));
        }
        Err(e) => {
            made.undo(&conn).await;
            return Err(e.into());
        }
    }
    let set = BTreeMap::from([
        ("S3_BUCKET".to_owned(), bucket.clone()),
        ("BUCKET_NAME".to_owned(), "${{S3_BUCKET}}".to_owned()),
        ("S3_ENDPOINT".to_owned(), cloudflare::r2_endpoint(&account)),
        ("S3_REGION".to_owned(), "auto".to_owned()),
        ("S3_FORCE_PATH_STYLE".to_owned(), "true".to_owned()),
        ("AWS_ACCESS_KEY_ID".to_owned(), creds.access_key_id.clone()),
        (
            "AWS_SECRET_ACCESS_KEY".to_owned(),
            creds.secret_access_key.clone(),
        ),
    ]);
    let vars = UpdateVariables {
        set,
        sealed: BTreeSet::from(["AWS_SECRET_ACCESS_KEY".to_owned()]),
        delete: BTreeSet::new(),
    };
    if let Err(e) = variables::apply_to_instance(state, actor, grant, meta, instance, vars).await {
        made.undo(&conn).await;
        sqlx::query("DELETE FROM r2_buckets WHERE service_instance_id = $1")
            .bind(instance)
            .execute(&state.db)
            .await?;
        return Err(e);
    }
    Entry::new("bucket.provision", meta)
        .actor(actor)
        .workspace(workspace)
        .target("service_instance", instance)
        .metadata(json!({
            "provider": "r2",
            "accountId": account,
            "bucket": bucket,
            "created": created,
            "tokenId": creds.token_id,
        }))
        .record(&state.db)
        .await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateBucket {
    /// The service's name on the canvas.
    pub name: String,
    /// The environment that gets its bucket now (others can add theirs later).
    pub environment_id: Id,
    #[serde(default)]
    pub bucket: Option<String>,
    #[serde(default)]
    pub existing: Option<String>,
}

/// Adds an R2 bucket service to a project: one instance per environment, and
/// a bucket (new or linked) for `environmentId`.
pub async fn create(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<CreateBucket>,
) -> AppResult<Json<CreatedService>> {
    let project = grant.id();
    let workspace = grant.workspace_id()?;
    validate_service_name(&req.name)?;
    let pick = Choice {
        bucket: req.bucket,
        existing: req.existing,
    }
    .pick()?;
    // Fail early, before the service exists, when R2 can't work at all.
    require_connection(&state, workspace).await?;
    let envs: Vec<Id> =
        sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1 ORDER BY created_at")
            .bind(project)
            .fetch_all(&state.db)
            .await?;
    if !envs.contains(&req.environment_id) {
        return Err(AppError::bad_request(
            "environmentId must be an environment of this project",
        ));
    }
    let config = serde_json::to_value(r2_config()).map_err(anyhow::Error::from)?;
    let id = Id::new();
    let instances: Vec<(Id, Id)> = envs.iter().map(|e| (*e, Id::new())).collect();

    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO services (id, project_id, name, template) VALUES ($1, $2, $3, $4)
         ON CONFLICT (project_id, name) DO NOTHING",
    )
    .bind(id)
    .bind(project)
    .bind(&req.name)
    .bind(R2_TEMPLATE)
    .execute(&mut *tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Err(AppError::Conflict(format!(
            "a service named {} already exists",
            req.name
        )));
    }
    for (env, instance) in &instances {
        sqlx::query(
            "INSERT INTO service_instances (id, service_id, environment_id, config) VALUES ($1, $2, $3, $4)",
        )
        .bind(instance)
        .bind(id)
        .bind(env)
        .bind(&config)
        .execute(&mut *tx)
        .await?;
        let created = [Change::new("service", "created", req.name.as_str())];
        activity::record(&mut tx, *env, Some(id), Some(actor.user_id), &created).await?;
    }
    Entry::new("service.create", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service", id)
        .metadata(json!({
            "project": project,
            "name": req.name,
            "source": "Cloudflare R2",
            "template": R2_TEMPLATE,
        }))
        .record(&mut *tx)
        .await?;
    tx.commit().await?;

    let instance = instances
        .iter()
        .find(|(e, _)| *e == req.environment_id)
        .map(|(_, i)| *i)
        .expect("checked above");
    if let Err(e) = provision(&state, &actor, &grant, &meta, instance, pick).await {
        // Nothing at Cloudflare is left behind; neither is the empty service.
        sqlx::query("DELETE FROM services WHERE id = $1")
            .bind(id)
            .execute(&state.db)
            .await?;
        return Err(e);
    }
    let mut loaded = Vec::with_capacity(instances.len());
    for (_, i) in instances {
        loaded.push(load_instance(&state, i).await?);
    }
    Ok(Json(CreatedService {
        id,
        name: req.name,
        instances: loaded,
    }))
}

/// Gives an R2 instance without a bucket (e.g. in a newer environment) one.
pub async fn provision_instance(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Choice>,
) -> AppResult<Json<crate::api::services::Instance>> {
    let instance = grant.id();
    provision(&state, &actor, &grant, &meta, instance, req.pick()?).await?;
    load_instance(&state, instance).await.map(Json)
}

#[derive(sqlx::FromRow)]
struct Row {
    account_id: String,
    bucket: String,
    token_id: String,
    created: bool,
}

async fn row(state: &AppState, instance: Id) -> AppResult<Row> {
    sqlx::query_as(
        "SELECT account_id, bucket, token_id, created FROM r2_buckets WHERE service_instance_id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| AppError::Conflict("this environment has no bucket".into()))
}

/// Forgets an instance's bucket: its row and variables.
async fn forget(
    state: &AppState,
    actor: &Actor,
    grant: &Grant,
    meta: &RequestMeta,
    instance: Id,
) -> AppResult<()> {
    let vars = UpdateVariables {
        set: BTreeMap::new(),
        sealed: BTreeSet::new(),
        delete: VARIABLES.iter().map(|v| (*v).to_owned()).collect(),
    };
    variables::apply_to_instance(state, actor, grant, meta, instance, vars).await?;
    sqlx::query("DELETE FROM r2_buckets WHERE service_instance_id = $1")
        .bind(instance)
        .execute(&state.db)
        .await?;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Released {
    pub ok: bool,
    /// Tokens left at Cloudflare because it isn't connected any more.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

fn orphaned_token(bucket: &str) -> String {
    format!(
        "Cloudflare isn't connected, so the API token for bucket {bucket} is still active: delete it under Manage Account → Account API Tokens in the Cloudflare dashboard"
    )
}

/// Unlinks an instance's bucket: revokes its token and removes the
/// variables. The bucket and its data stay at Cloudflare.
pub async fn unlink(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Released>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    let r = row(&state, instance).await?;
    let mut warnings = Vec::new();
    match cloudflare::connection(&state, workspace).await? {
        Some(conn) => conn
            .client
            .delete_account_token(&r.account_id, &r.token_id)
            .await
            .map_err(token_error)?,
        None => warnings.push(orphaned_token(&r.bucket)),
    }
    forget(&state, &actor, &grant, &meta, instance).await?;
    Entry::new("bucket.unlink", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", instance)
        .metadata(json!({ "provider": "r2", "bucket": r.bucket, "tokenId": r.token_id }))
        .record(&state.db)
        .await?;
    Ok(Json(Released { ok: true, warnings }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteBucket {
    /// The bucket's name, typed by the user.
    pub confirm: String,
}

/// Deletes an instance's bucket at Cloudflare (only one Modula created),
/// then its token and variables. Cloudflare only deletes empty buckets.
pub async fn delete_bucket(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<DeleteBucket>,
) -> AppResult<Json<Released>> {
    let instance = grant.id();
    let workspace = grant.workspace_id()?;
    let r = row(&state, instance).await?;
    if !r.created {
        return Err(AppError::Conflict(format!(
            "{} was linked, not created here: delete it in the Cloudflare dashboard, or unlink it",
            r.bucket
        )));
    }
    if req.confirm.trim() != r.bucket {
        return Err(AppError::bad_request(
            "type the bucket's name to confirm deleting it",
        ));
    }
    let (conn, _) = require_connection(&state, workspace).await?;
    // The bucket first: if Cloudflare refuses, nothing has changed.
    if let Err(e) = conn.client.delete_r2_bucket(&r.account_id, &r.bucket).await {
        return Err(match e.status() {
            Some(409) => AppError::Conflict(format!(
                "{} isn't empty, and Cloudflare only deletes empty buckets: empty it (in the Cloudflare dashboard or with an S3 client), then try again",
                r.bucket
            )),
            _ => r2_error(e),
        });
    }
    conn.client
        .delete_account_token(&r.account_id, &r.token_id)
        .await
        .map_err(token_error)?;
    forget(&state, &actor, &grant, &meta, instance).await?;
    Entry::new("bucket.delete", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("service_instance", instance)
        .metadata(json!({ "provider": "r2", "bucket": r.bucket, "accountId": r.account_id }))
        .record(&state.db)
        .await?;
    Ok(Json(Released {
        ok: true,
        warnings: Vec::new(),
    }))
}

/// Whose bucket tokens [`release`] revokes.
#[derive(Debug, Clone, Copy)]
pub enum Scope {
    Service(Id),
    Environment(Id),
    Project(Id),
    Workspace(Id),
}

/// Revokes the bucket tokens of everything in `scope`, before it is deleted
/// (the buckets stay). Errors from Cloudflare are returned (retrying is safe:
/// a token already gone counts as revoked); a workspace without Cloudflare
/// connected gets warnings instead, since nothing can be revoked.
pub async fn release(state: &AppState, scope: Scope) -> AppResult<Vec<String>> {
    let (kind, id) = match scope {
        Scope::Service(id) => ("service", id),
        Scope::Environment(id) => ("environment", id),
        Scope::Project(id) => ("project", id),
        Scope::Workspace(id) => ("workspace", id),
    };
    let rows: Vec<(Id, String, String, String)> = sqlx::query_as(
        "SELECT p.workspace_id, r.account_id, r.token_id, r.bucket FROM r2_buckets r
         JOIN service_instances i ON i.id = r.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         WHERE $1 = CASE $2 WHEN 'service' THEN s.id WHEN 'environment' THEN i.environment_id
                            WHEN 'project' THEN p.id ELSE p.workspace_id END",
    )
    .bind(id)
    .bind(kind)
    .fetch_all(&state.db)
    .await?;
    let Some((workspace, ..)) = rows.first() else {
        return Ok(Vec::new());
    };
    let Some(conn) = cloudflare::connection(state, *workspace).await? else {
        return Ok(rows.iter().map(|r| orphaned_token(&r.3)).collect());
    };
    for (_, account, token, _) in &rows {
        conn.client
            .delete_account_token(account, token)
            .await
            .map_err(token_error)?;
    }
    Ok(Vec::new())
}

/// [`release`] for bulk deletions (environments, projects, workspaces), which
/// go ahead regardless: failures are logged.
pub async fn release_best_effort(state: &AppState, scope: Scope) {
    match release(state, scope).await {
        Ok(w) if w.is_empty() => {}
        Ok(w) => tracing::warn!(?scope, warnings = ?w, "R2 bucket tokens left at Cloudflare"),
        Err(e) => tracing::warn!(?scope, error = %e, "could not revoke R2 bucket tokens"),
    }
}

/// An R2 bucket in the account, and where it is used.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBucket {
    pub name: String,
    pub creation_date: Option<String>,
    pub location: Option<String>,
    pub links: Vec<Link>,
}

#[derive(Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    #[serde(skip)]
    pub bucket: String,
    pub project_id: Id,
    pub project_name: String,
    pub service_id: Id,
    pub service_name: String,
    pub environment_id: Id,
    pub environment_name: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountBuckets {
    pub account_id: String,
    pub endpoint: String,
    pub buckets: Vec<AccountBucket>,
}

/// The account's R2 buckets, live from Cloudflare, with the services using
/// each (for linking an existing bucket).
pub async fn list(State(state): State<AppState>, grant: Grant) -> AppResult<Json<AccountBuckets>> {
    let workspace = grant.id();
    let (conn, account) = require_connection(&state, workspace).await?;
    let buckets = conn.client.r2_buckets(&account).await.map_err(r2_error)?;
    let links: Vec<Link> = sqlx::query_as(
        "SELECT r.bucket, p.id AS project_id, p.name AS project_name, s.id AS service_id,
                s.name AS service_name, e.id AS environment_id, e.name AS environment_name
         FROM r2_buckets r
         JOIN service_instances i ON i.id = r.service_instance_id
         JOIN services s ON s.id = i.service_id
         JOIN environments e ON e.id = i.environment_id
         JOIN projects p ON p.id = s.project_id
         WHERE p.workspace_id = $1 AND r.account_id = $2
         ORDER BY p.name, s.name, e.name",
    )
    .bind(workspace)
    .bind(&account)
    .fetch_all(&state.db)
    .await?;
    let mut by_bucket: BTreeMap<String, Vec<Link>> = BTreeMap::new();
    for l in links {
        by_bucket.entry(l.bucket.clone()).or_default().push(l);
    }
    Ok(Json(AccountBuckets {
        endpoint: cloudflare::r2_endpoint(&account),
        account_id: account,
        buckets: buckets
            .into_iter()
            .map(|b| AccountBucket {
                links: by_bucket.remove(&b.name).unwrap_or_default(),
                name: b.name,
                creation_date: b.creation_date,
                location: b.location,
            })
            .collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::config::Source;

    #[test]
    fn choices() {
        let c = |b: Option<&str>, e: Option<&str>| Choice {
            bucket: b.map(Into::into),
            existing: e.map(Into::into),
        };
        assert!(matches!(c(Some(" media "), None).pick(), Ok(Pick::New(n)) if n == "media"));
        assert!(matches!(c(Some(""), Some("old")).pick(), Ok(Pick::Existing(n)) if n == "old"));
        assert!(c(None, None).pick().is_err());
        assert!(c(Some("a"), Some("b")).pick().is_err());
        assert!(c(Some("Bad_Name"), None).pick().is_err());
        assert!(!r2_config().deployable());
        assert!(matches!(r2_config().source, Source::R2));
    }
}
