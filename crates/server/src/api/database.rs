//! The Database tab of a Postgres service: tables and rows, a query editor,
//! stats, extensions, credentials and password rotation. Credentials work for
//! every database template; everything else is Postgres-only.
//!
//! The control plane never connects to user databases. Each operation goes to
//! the node running the service's active deployment as a `DbRequest`; the node
//! connects over the environment network with the deployment's own
//! credentials and answers with JSON that is passed through as the response
//! (see `modula_node::db`). Postgres errors come back as 400s with Postgres's
//! message. Every write and every query run is audited (the first 500
//! characters of SQL; never results).

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use modula_id::Id;
use modula_proto::node::v1 as pb;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::activity::{self, Change};
use crate::api::variables::{self, UpdateVariables};
use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::{Action, Grant, Scope};
use crate::deploy::{self, env};
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::nodes::hub::DbCallError;
use crate::state::AppState;
use crate::templates;

/// Also the session's statement_timeout on the node.
const TIMEOUT: Duration = Duration::from_secs(30);
/// VACUUM rewrites visibility data for the whole table.
const VACUUM_TIMEOUT: Duration = Duration::from_secs(300);
/// Above the node's own limits, so its error (not ours) explains a timeout.
const GRACE: Duration = Duration::from_secs(5);
const AUDIT_SQL_CHARS: usize = 500;
const MAX_SQL_BYTES: usize = 1024 * 1024;
const MAX_PAGE_SIZE: u32 = 500;
/// Postgres's own limit on columns per table.
const MAX_COLUMNS: usize = 1600;

/// 409 unless the instance's service was created from the Postgres template.
async fn postgres_only(state: &AppState, instance: Id) -> AppResult<()> {
    let template: Option<String> = sqlx::query_scalar(
        "SELECT s.template FROM service_instances i JOIN services s ON s.id = i.service_id
         WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    if template.as_deref() != Some("postgres") {
        return Err(AppError::Conflict(
            "the Database tab is only available for Postgres services".into(),
        ));
    }
    Ok(())
}

/// The active deployment of a Postgres instance and the node running it.
struct Live {
    deployment: Id,
    node: Id,
}

async fn live(state: &AppState, instance: Id) -> AppResult<Live> {
    postgres_only(state, instance).await?;
    let row: Option<(Id, Id, bool)> = sqlx::query_as(
        "SELECT d.id, n.id, coalesce(n.info->'capabilities' ? 'db', false)
         FROM deployments d JOIN nodes n ON n.id = d.node_id
         WHERE d.service_instance_id = $1 AND d.status = 'active'
         ORDER BY d.created_at DESC LIMIT 1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let Some((deployment, node, capable)) = row else {
        return Err(AppError::Conflict(
            "the database is not running; deploy it first".into(),
        ));
    };
    if !capable {
        return Err(AppError::Conflict(
            "the server running this database needs a newer modula-node".into(),
        ));
    }
    Ok(Live { deployment, node })
}

/// Run one operation on the database's node.
async fn call(
    state: &AppState,
    live: &Live,
    op: &str,
    args: Value,
    timeout: Duration,
) -> AppResult<Value> {
    let req = pb::DbRequest {
        request_id: String::new(),
        deployment_id: live.deployment.to_string(),
        op: op.to_owned(),
        json_args: args.to_string(),
        timeout_ms: timeout.as_millis() as u32,
    };
    match state.hub.db_call(live.node, req, timeout + GRACE).await {
        Ok(r) if r.error.is_empty() => serde_json::from_str(&r.json)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("invalid database answer: {e}"))),
        Ok(r) if r.bad_request => Err(AppError::BadRequest(r.error)),
        Ok(r) => Err(AppError::Conflict(r.error)),
        Err(DbCallError::Offline) => Err(AppError::Conflict(
            "the server running this database is offline".into(),
        )),
        Err(DbCallError::TimedOut) => Err(AppError::Conflict(
            "the database did not answer in time".into(),
        )),
    }
}

/// Look up the live database, record the audit entry, then run `op`.
#[allow(clippy::too_many_arguments)]
async fn audited_call(
    state: &AppState,
    actor: &Actor,
    grant: &Grant,
    meta: &RequestMeta,
    action: &'static str,
    metadata: Value,
    op: &str,
    args: Value,
    timeout: Duration,
) -> AppResult<Value> {
    let instance = grant.id();
    let live = live(state, instance).await?;
    // Recorded before running, so failed attempts are on record too.
    Entry::new(action, meta)
        .actor(actor)
        .workspace(grant.workspace_id()?)
        .target("service_instance", instance)
        .metadata(metadata)
        .record(&state.db)
        .await?;
    call(state, &live, op, args, timeout).await
}

fn validate_table(table: &str) -> AppResult<()> {
    // Two identifiers of at most 63 bytes and a dot; the node resolves it.
    if table.is_empty() || table.len() > 127 || table.contains('\0') {
        return Err(AppError::bad_request("invalid table name"));
    }
    Ok(())
}

fn validate_columns<'a>(names: impl IntoIterator<Item = &'a String>) -> AppResult<()> {
    let mut n = 0;
    for name in names {
        n += 1;
        if name.is_empty() || name.len() > 63 || name.contains('\0') {
            return Err(AppError::bad_request("invalid column name"));
        }
    }
    if n > MAX_COLUMNS {
        return Err(AppError::bad_request("too many columns"));
    }
    Ok(())
}

// ------------------------------------------------------------------ browsing

/// `{ tables: [{ schema, name, rowEstimate }] }`
pub async fn tables(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Value>> {
    let live = live(&state, grant.id()).await?;
    call(&state, &live, "tables", Value::Null, TIMEOUT)
        .await
        .map(Json)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RowsQuery {
    /// `schema.name` or `name` (public first).
    pub table: String,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
    pub order_by: Option<String>,
    /// `asc` (default) or `desc`.
    pub dir: Option<String>,
}

/// `{ columns: [{ name, type, nullable, primaryKey }], rows: (string|null)[][],
/// total, page, pageSize, truncated }`
pub async fn rows(
    State(state): State<AppState>,
    grant: Grant,
    Query(q): Query<RowsQuery>,
) -> AppResult<Json<Value>> {
    validate_table(&q.table)?;
    if let Some(c) = &q.order_by {
        validate_columns([c])?;
    }
    let page_size = q.page_size.unwrap_or(50);
    if page_size == 0 || page_size > MAX_PAGE_SIZE {
        return Err(AppError::bad_request("pageSize must be 1–500"));
    }
    let desc = match q.dir.as_deref() {
        None | Some("asc") => false,
        Some("desc") => true,
        Some(_) => return Err(AppError::bad_request("dir must be asc or desc")),
    };
    let live = live(&state, grant.id()).await?;
    let args = json!({
        "table": q.table,
        "page": q.page.unwrap_or(1).max(1),
        "pageSize": page_size,
        "orderBy": q.order_by,
        "desc": desc,
    });
    call(&state, &live, "rows", args, TIMEOUT).await.map(Json)
}

/// `{ connections, cacheHitRatio, sizes, tables, unusedIndexes,
/// pgStatStatements, topQueries }`
pub async fn stats(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Value>> {
    let live = live(&state, grant.id()).await?;
    call(&state, &live, "stats", Value::Null, TIMEOUT)
        .await
        .map(Json)
}

/// `{ installed: [{ name, version, comment }], available: [...] }`
pub async fn extensions(State(state): State<AppState>, grant: Grant) -> AppResult<Json<Value>> {
    let live = live(&state, grant.id()).await?;
    call(&state, &live, "extensions", Value::Null, TIMEOUT)
        .await
        .map(Json)
}

// ------------------------------------------------------------------ writes

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunQuery {
    pub sql: String,
    /// One read-only statement (SELECT, WITH, VALUES, TABLE, EXPLAIN, SHOW)
    /// run in a READ ONLY transaction that is rolled back. What AI agents and
    /// the CLI use by default.
    #[serde(default)]
    pub read_only: bool,
}

/// The route is authorized for [`Action::DatabaseRead`]; anything else a query
/// can do (writes, DDL) needs what [`Action::DatabaseWrite`] needs.
fn require_write(actor: &Actor, grant: &Grant) -> AppResult<()> {
    let action = Action::DatabaseWrite;
    match grant.scope {
        Scope::Workspace { role, .. } if role >= action.min_role() => {}
        _ => return Err(AppError::Forbidden),
    }
    if action.requires_sudo() && !actor.sudo_active() {
        return Err(AppError::SudoRequired);
    }
    Ok(())
}

/// `{ columns: [{ name, type }], rows, rowCount, command, durationMs, truncated }`:
/// the last result set of the script (at most 1000 rows). With `readOnly`,
/// exactly one statement, which can't change anything.
pub async fn query(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<RunQuery>,
) -> AppResult<Json<Value>> {
    if req.sql.trim().is_empty() {
        return Err(AppError::bad_request("the query is empty"));
    }
    if req.sql.len() > MAX_SQL_BYTES || req.sql.contains('\0') {
        return Err(AppError::bad_request(
            "the query must be under 1 MB and contain no NUL bytes",
        ));
    }
    if !req.read_only {
        require_write(&actor, &grant)?;
    }
    let sql: String = req.sql.chars().take(AUDIT_SQL_CHARS).collect();
    audited_call(
        &state,
        &actor,
        &grant,
        &meta,
        "database.query",
        json!({ "sql": sql, "readOnly": req.read_only }),
        "query",
        json!({ "sql": req.sql, "readOnly": req.read_only }),
        TIMEOUT,
    )
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertRow {
    pub table: String,
    pub values: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateRow {
    pub table: String,
    /// The primary key's columns, exactly.
    pub key: BTreeMap<String, String>,
    pub values: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteRow {
    pub table: String,
    pub key: BTreeMap<String, String>,
}

/// `{ rowCount }`. Values are text, cast to each column's type.
pub async fn insert_row(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<InsertRow>,
) -> AppResult<Json<Value>> {
    validate_table(&req.table)?;
    validate_columns(req.values.keys())?;
    // Column names only: rows may hold personal data.
    let metadata = json!({ "table": req.table, "columns": req.values.keys().collect::<Vec<_>>() });
    let args = json!({ "table": req.table, "values": req.values });
    audited_call(
        &state,
        &actor,
        &grant,
        &meta,
        "database.row.insert",
        metadata,
        "insert",
        args,
        TIMEOUT,
    )
    .await
    .map(Json)
}

/// `{ rowCount }`. Only tables with a primary key.
pub async fn update_row(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<UpdateRow>,
) -> AppResult<Json<Value>> {
    validate_table(&req.table)?;
    validate_columns(req.values.keys().chain(req.key.keys()))?;
    if req.key.is_empty() || req.values.is_empty() {
        return Err(AppError::bad_request("key and values are required"));
    }
    let metadata = json!({ "table": req.table, "columns": req.values.keys().collect::<Vec<_>>() });
    let args = json!({ "table": req.table, "key": req.key, "values": req.values });
    audited_call(
        &state,
        &actor,
        &grant,
        &meta,
        "database.row.update",
        metadata,
        "update",
        args,
        TIMEOUT,
    )
    .await
    .map(Json)
}

/// `{ rowCount }`. Only tables with a primary key.
pub async fn delete_row(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<DeleteRow>,
) -> AppResult<Json<Value>> {
    validate_table(&req.table)?;
    validate_columns(req.key.keys())?;
    if req.key.is_empty() {
        return Err(AppError::bad_request("key is required"));
    }
    let args = json!({ "table": req.table, "key": req.key });
    audited_call(
        &state,
        &actor,
        &grant,
        &meta,
        "database.row.delete",
        json!({ "table": req.table }),
        "delete",
        args,
        TIMEOUT,
    )
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vacuum {
    pub table: String,
}

/// `VACUUM (ANALYZE)` one table. `{ ok: true }`
pub async fn vacuum(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<Vacuum>,
) -> AppResult<Json<Value>> {
    validate_table(&req.table)?;
    audited_call(
        &state,
        &actor,
        &grant,
        &meta,
        "database.vacuum",
        json!({ "table": req.table }),
        "vacuum",
        json!({ "table": req.table }),
        VACUUM_TIMEOUT,
    )
    .await
    .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionChange {
    pub name: String,
    /// `install` or `uninstall`.
    pub action: String,
}

/// Install or uninstall one of `pg_available_extensions`; answers like GET.
pub async fn extension(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    Json(req): Json<ExtensionChange>,
) -> AppResult<Json<Value>> {
    if req.name.is_empty() || req.name.len() > 63 || req.name.contains('\0') {
        return Err(AppError::bad_request("invalid extension name"));
    }
    let (install, action) = match req.action.as_str() {
        "install" => (true, "database.extension.install"),
        "uninstall" => (false, "database.extension.uninstall"),
        _ => return Err(AppError::bad_request("action must be install or uninstall")),
    };
    audited_call(
        &state,
        &actor,
        &grant,
        &meta,
        action,
        json!({ "name": req.name }),
        "extension",
        json!({ "name": req.name, "install": install }),
        VACUUM_TIMEOUT,
    )
    .await
    .map(Json)
}

// ------------------------------------------------------------------ credentials

/// Where each template keeps its connection details (see `templates.rs`).
struct Engine {
    scheme: &'static str,
    /// The variable holding the user name, else the fixed name.
    user_var: Option<&'static str>,
    default_user: &'static str,
    password_var: &'static str,
    database_var: Option<&'static str>,
    private_url_var: &'static str,
    public_url_var: &'static str,
    /// Appended to a public URL built here (Postgres requires TLS publicly).
    public_suffix: &'static str,
}

fn engine(template: &str) -> Option<Engine> {
    Some(match template {
        "postgres" => Engine {
            scheme: "postgresql",
            user_var: Some("POSTGRES_USER"),
            default_user: "postgres",
            password_var: "POSTGRES_PASSWORD",
            database_var: Some("POSTGRES_DB"),
            private_url_var: "DATABASE_URL",
            public_url_var: "DATABASE_PUBLIC_URL",
            public_suffix: "?sslmode=require",
        },
        "mysql" => Engine {
            scheme: "mysql",
            user_var: None,
            default_user: "root",
            password_var: "MYSQL_ROOT_PASSWORD",
            database_var: Some("MYSQL_DATABASE"),
            private_url_var: "MYSQL_URL",
            public_url_var: "MYSQL_PUBLIC_URL",
            public_suffix: "",
        },
        "redis" => Engine {
            scheme: "redis",
            user_var: None,
            default_user: "default",
            password_var: "REDIS_PASSWORD",
            database_var: None,
            private_url_var: "REDIS_URL",
            public_url_var: "REDIS_PUBLIC_URL",
            public_suffix: "",
        },
        "mongo" => Engine {
            scheme: "mongodb",
            user_var: Some("MONGO_INITDB_ROOT_USERNAME"),
            default_user: "mongo",
            password_var: "MONGO_INITDB_ROOT_PASSWORD",
            database_var: None,
            private_url_var: "MONGO_URL",
            public_url_var: "MONGO_PUBLIC_URL",
            public_suffix: "",
        },
        _ => return None,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    /// `postgres`, `mysql`, `redis` or `mongo`.
    pub template: String,
    pub username: String,
    pub password: String,
    /// None for engines without one in the URL (Redis, MongoDB).
    pub database: Option<String>,
    /// The private hostname inside the environment's network.
    pub host: String,
    pub port: u16,
    pub private_url: String,
    /// Through the TCP proxy (TLS required for Postgres); null without one.
    pub public_url: Option<String>,
    pub public_host: Option<String>,
    pub public_port: Option<u16>,
}

/// A database instance's credentials, from its resolved variables (so they
/// follow edits to the variables, deployed or not).
async fn resolve_credentials(state: &AppState, instance: Id) -> AppResult<Credentials> {
    let (environment, service, template): (Id, String, Option<String>) = sqlx::query_as(
        "SELECT i.environment_id, s.name, s.template FROM service_instances i
         JOIN services s ON s.id = i.service_id WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    let not_database =
        || AppError::Conflict("only database services (from a template) have credentials".into());
    let template = template.ok_or_else(not_database)?;
    let (engine, port) = engine(&template)
        .zip(templates::get(&template).map(|t| t.port))
        .ok_or_else(not_database)?;
    let resolved = env::resolve(
        &state.keys,
        &state.db,
        state.cfg.tcp_proxy_host.as_deref(),
        environment,
        instance,
    )
    .await?;
    let var = |k: &str| {
        resolved
            .vars
            .get(k)
            .map(|v| v.expose_secret().to_owned())
            .filter(|v| !v.is_empty())
    };
    let username = engine
        .user_var
        .and_then(var)
        .unwrap_or_else(|| engine.default_user.into());
    let password = var(engine.password_var).ok_or_else(|| {
        AppError::Conflict(format!(
            "this database has no {} variable",
            engine.password_var
        ))
    })?;
    // Postgres defaults the database to the user name, like the image does.
    let database = engine.database_var.map(|k| {
        var(k).unwrap_or_else(|| {
            if template == "postgres" {
                username.clone()
            } else {
                String::new()
            }
        })
    });
    let database = database.filter(|d| !d.is_empty());
    let host = format!("{service}.modula.internal");
    let enc = |s: &str| utf8_percent_encode(s, NON_ALPHANUMERIC).to_string();
    let url = |host: &str, port: u16| {
        let path = database
            .as_deref()
            .map(|d| format!("/{}", enc(d)))
            .unwrap_or_default();
        format!(
            "{}://{}:{}@{host}:{port}{path}",
            engine.scheme,
            enc(&username),
            enc(&password)
        )
    };
    let private_url = var(engine.private_url_var).unwrap_or_else(|| url(&host, port));
    let proxy: Option<(i32, Option<String>)> = sqlx::query_as(
        "SELECT t.public_port, n.public_ip FROM tcp_proxies t JOIN nodes n ON n.id = t.node_id
         WHERE t.service_instance_id = $1",
    )
    .bind(instance)
    .fetch_optional(&state.db)
    .await?;
    let public = proxy.and_then(|(port, ip)| {
        let host = state.cfg.tcp_proxy_host.clone().or(ip)?;
        Some((host, u16::try_from(port).ok()?))
    });
    let public_url = public.as_ref().map(|(h, p)| {
        var(engine.public_url_var)
            .unwrap_or_else(|| format!("{}{}", url(h, *p), engine.public_suffix))
    });
    Ok(Credentials {
        template,
        username,
        password,
        database,
        host,
        port,
        private_url,
        public_url,
        public_host: public.as_ref().map(|(h, _)| h.clone()),
        public_port: public.map(|(_, p)| p),
    })
}

/// The connection details of any database service (for the CLI's `connect`
/// and the Config view). Member+, audited; no sudo, so API tokens work.
pub async fn credentials(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Credentials>> {
    let instance = grant.id();
    let creds = resolve_credentials(&state, instance).await?;
    Entry::new("database.credentials.read", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service_instance", instance)
        .metadata(json!({ "template": creds.template }))
        .record(&state.db)
        .await?;
    Ok(Json(creds))
}

/// Rotate the password: `ALTER USER` on the live database (through the node),
/// then store it in POSTGRES_PASSWORD. Services referencing this database get
/// a pending change and pick the new URL up on their next deploy. The running
/// deployment's environment snapshot is updated too, so the node keeps
/// connecting (it reads credentials from desired state) without a restart.
pub async fn regenerate_password(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
) -> AppResult<Json<Credentials>> {
    let instance = grant.id();
    let live = live(&state, instance).await?;
    let (environment, service, snapshot): (Id, String, Option<Vec<u8>>) = sqlx::query_as(
        "SELECT i.environment_id, s.name, d.env_sealed FROM deployments d
         JOIN service_instances i ON i.id = d.service_instance_id
         JOIN services s ON s.id = i.service_id WHERE d.id = $1",
    )
    .bind(live.deployment)
    .fetch_one(&state.db)
    .await?;
    let mut vars = match &snapshot {
        Some(s) => {
            env::open_snapshot(&state.keys, &state.db, environment, live.deployment, s).await?
        }
        None => env::EnvMap::new(),
    };
    let old = vars
        .get("POSTGRES_PASSWORD")
        .map(|v| v.expose_secret().to_owned())
        .unwrap_or_default();
    let new = templates::password();

    Entry::new("database.password.regenerate", &meta)
        .actor(&actor)
        .workspace(grant.workspace_id()?)
        .target("service_instance", instance)
        .record(&state.db)
        .await?;
    call(
        &state,
        &live,
        "password",
        json!({ "password": new }),
        TIMEOUT,
    )
    .await?;

    // The database has the new password now; losing it would lock every
    // client out, so on failure put the old one back.
    let set = UpdateVariables {
        set: BTreeMap::from([("POSTGRES_PASSWORD".to_owned(), new.clone())]),
        sealed: BTreeSet::new(),
        delete: BTreeSet::new(),
    };
    if let Err(e) = variables::update_instance_variables(
        State(state.clone()),
        actor.clone(),
        grant,
        meta.clone(),
        Json(set),
    )
    .await
    {
        let revert = call(
            &state,
            &live,
            "password",
            json!({ "password": old }),
            TIMEOUT,
        )
        .await;
        if let Err(r) = revert {
            tracing::error!(%instance, error = %r, "could not restore the database password");
        }
        return Err(e);
    }

    // The snapshot's values that embed the password (PGPASSWORD, the URLs).
    if !old.is_empty() {
        for v in vars.values_mut() {
            if v.expose_secret().contains(&old) {
                *v = SecretString::from(v.expose_secret().replace(&old, &new));
            }
        }
    }
    let sealed =
        env::seal_snapshot(&state.keys, &state.db, environment, live.deployment, &vars).await?;
    drop(vars);
    // And the variables as written, so the new password isn't a pending change
    // (and discarding one can't bring the old password back).
    let raw = env::raw_variables(&state.keys, &state.db, environment, instance).await?;
    let vars_sealed =
        env::seal_vars_snapshot(&state.keys, &state.db, environment, live.deployment, &raw).await?;
    drop(raw);
    let mut tx = state.db.begin().await?;
    sqlx::query("UPDATE deployments SET env_sealed = $2, vars_sealed = $3 WHERE id = $1")
        .bind(live.deployment)
        .bind(&sealed)
        .bind(&vars_sealed)
        .execute(&mut *tx)
        .await?;
    deploy::bump(&mut *tx, live.node).await?;
    // Services whose variables reference this one (`${{postgres.…}}`).
    let prefix = format!("{service}.");
    let dependents: Vec<Id> = sqlx::query_scalar(
        "SELECT DISTINCT i.service_id FROM variables v
         JOIN service_instances i ON i.id = v.service_instance_id
         WHERE v.environment_id = $1 AND i.id <> $2
           AND EXISTS (SELECT 1 FROM unnest(v.refs) r WHERE left(r, length($3)) = $3)",
    )
    .bind(environment)
    .bind(instance)
    .bind(&prefix)
    .fetch_all(&mut *tx)
    .await?;
    let change = [Change::new(
        "variable",
        "updated",
        format!("{service}.POSTGRES_PASSWORD"),
    )];
    for dependent in dependents {
        activity::record(
            &mut tx,
            environment,
            Some(dependent),
            Some(actor.user_id),
            &change,
        )
        .await?;
    }
    tx.commit().await?;
    state.hub.kick(live.node);
    resolve_credentials(&state, instance).await.map(Json)
}
