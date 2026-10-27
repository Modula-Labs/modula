//! The Database tab: browse, query and maintain a Postgres service.
//!
//! The server never connects to user databases. It sends a `DbRequest` over
//! the node stream; this node (which runs the database container) connects to
//! the container's address on its environment network with the credentials
//! from the deployment's own environment, runs one operation and answers with
//! JSON. One short-lived connection per request, with `statement_timeout` set.
//!
//! Credentials come from the newest desired state's workload (falling back to
//! the container's environment), so a password the server rotated reaches the
//! node with the next snapshot, without restarting the database.
//!
//! Reads (tables, rows, stats, extensions, read-only queries) don't use
//! those credentials: they connect as `modula_reader`, a role this node
//! keeps in each database with read-only privileges (see [`pg`]).

pub mod fake;
pub mod pg;
pub mod sql;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use modula_proto::node::v1 as pb;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::runtime::Runtime;

/// Postgres listens here inside the container (the template's port).
const PORT: u16 = 5432;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// Results are cut well below the stream's 4 MiB message limit.
pub const RESPONSE_BUDGET: usize = 2 * 1024 * 1024;

/// Where and as whom to connect. The password is never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Target {
    /// The database's container (whose roles the node has set up).
    pub container_id: String,
    /// Its service: what the reader's password is derived from.
    pub service_id: String,
    pub addr: SocketAddr,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("container_id", &self.container_id)
            .field("addr", &self.addr)
            .field("user", &self.user)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

/// One operation and its arguments: `{"op": "rows", "args": {…}}`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "camelCase")]
pub enum Op {
    Tables,
    Rows(RowsArgs),
    Query(QueryArgs),
    Insert(InsertArgs),
    Update(UpdateArgs),
    Delete(DeleteArgs),
    Stats,
    Vacuum(TableArgs),
    Extensions,
    Extension(ExtensionArgs),
    Password(PasswordArgs),
    /// Recovery and WAL archiving state (point-in-time recovery).
    WalStatus,
    /// Streaming replication: the standbys of a primary, or a standby's lag.
    Replication,
    /// Create (or set the password of) the role read replicas connect as.
    ReplicationRole(PasswordArgs),
    /// Turn a standby into a primary.
    Promote,
}

impl Op {
    pub fn parse(op: &str, json_args: &str) -> Result<Self, DbError> {
        let args: Value = if json_args.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(json_args)
                .map_err(|e| DbError::BadRequest(format!("invalid arguments: {e}")))?
        };
        serde_json::from_value(serde_json::json!({ "op": op, "args": args }))
            .map_err(|e| DbError::BadRequest(format!("invalid database operation: {e}")))
    }

    /// Only reads: runs as the read-only role.
    pub fn is_read(&self) -> bool {
        match self {
            Self::Tables
            | Self::Rows(_)
            | Self::Stats
            | Self::Extensions
            | Self::WalStatus
            | Self::Replication => true,
            Self::Query(a) => a.read_only,
            _ => false,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Tables => "tables",
            Self::Rows(_) => "rows",
            Self::Query(_) => "query",
            Self::Insert(_) => "insert",
            Self::Update(_) => "update",
            Self::Delete(_) => "delete",
            Self::Stats => "stats",
            Self::Vacuum(_) => "vacuum",
            Self::Extensions => "extensions",
            Self::Extension(_) => "extension",
            Self::Password(_) => "password",
            Self::WalStatus => "walStatus",
            Self::Replication => "replication",
            Self::ReplicationRole(_) => "replicationRole",
            Self::Promote => "promote",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RowsArgs {
    pub table: String,
    pub page: u32,
    pub page_size: u32,
    pub order_by: Option<String>,
    #[serde(default)]
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueryArgs {
    pub sql: String,
    /// One read-only statement inside a `READ ONLY` transaction, rolled back.
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InsertArgs {
    pub table: String,
    pub values: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateArgs {
    pub table: String,
    pub key: BTreeMap<String, String>,
    pub values: BTreeMap<String, Option<String>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    pub table: String,
    pub key: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableArgs {
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionArgs {
    pub name: String,
    pub install: bool,
}

#[derive(Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordArgs {
    pub password: String,
}

impl std::fmt::Debug for PasswordArgs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PasswordArgs { .. }")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DbError {
    /// The request's fault: a Postgres error or invalid arguments.
    #[error("{0}")]
    BadRequest(String),
    /// The database isn't running here or can't be reached.
    #[error("{0}")]
    Unavailable(String),
}

/// Runs operations against a database. [`pg::Postgres`] is the real one;
/// [`fake::FakeDb`] answers from canned data in tests.
#[async_trait]
pub trait Executor: Send + Sync + 'static {
    async fn run(&self, target: &Target, op: Op, timeout: Duration) -> Result<Value, DbError>;
}

/// The credentials in a Postgres container's environment.
fn credentials(env: &[(String, String)]) -> Result<(String, String, String), DbError> {
    let get = |k: &str| {
        env.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_empty())
    };
    let user = get("POSTGRES_USER").unwrap_or_else(|| "postgres".into());
    let password = get("POSTGRES_PASSWORD").ok_or_else(|| {
        DbError::Unavailable("the database has no POSTGRES_PASSWORD variable".into())
    })?;
    let database = get("POSTGRES_DB").unwrap_or_else(|| user.clone());
    Ok((user, password, database))
}

/// The address and credentials of a deployment's running database. `env` is
/// the deployment's workload environment from the newest desired state.
pub async fn target(
    runtime: &dyn Runtime,
    deployment_id: &str,
    env: Option<Vec<(String, String)>>,
) -> Result<Target, DbError> {
    let unavailable = |e: anyhow::Error| DbError::Unavailable(format!("{e:#}"));
    let container = runtime
        .containers()
        .await
        .map_err(unavailable)?
        .into_iter()
        .find(|c| c.deployment_id == deployment_id && c.running)
        .ok_or_else(|| DbError::Unavailable("the database is not running on this server".into()))?;
    let env = match env {
        Some(env) => env,
        None => runtime
            .container_env(&container.id)
            .await
            .map_err(unavailable)?,
    };
    let (user, password, database) = credentials(&env)?;
    let ip = runtime
        .address(&container.id, &container.environment_id)
        .await
        .map_err(unavailable)?
        .ok_or_else(|| DbError::Unavailable("the database has no network address yet".into()))?;
    Ok(Target {
        container_id: container.id.clone(),
        service_id: container.service_id.clone(),
        addr: SocketAddr::new(ip, PORT),
        user,
        password,
        database,
    })
}

/// Answer one `DbRequest` on `tx` (the session's stream).
pub async fn handle(
    runtime: Arc<dyn Runtime>,
    executor: Arc<dyn Executor>,
    env: Option<Vec<(String, String)>>,
    req: pb::DbRequest,
    tx: mpsc::Sender<pb::NodeMessage>,
) {
    let timeout = match req.timeout_ms {
        0 => DEFAULT_TIMEOUT,
        ms => Duration::from_millis(u64::from(ms)).clamp(Duration::from_secs(1), MAX_TIMEOUT),
    };
    let outcome = async {
        let op = Op::parse(&req.op, &req.json_args)?;
        let target = target(runtime.as_ref(), &req.deployment_id, env).await?;
        // A backstop above statement_timeout: connecting or a stuck socket.
        tokio::time::timeout(
            timeout + Duration::from_secs(2),
            executor.run(&target, op, timeout),
        )
        .await
        .unwrap_or_else(|_| {
            Err(DbError::Unavailable(
                "the database did not answer in time".into(),
            ))
        })
    }
    .await;
    let response = match outcome.and_then(|v| {
        let json = v.to_string();
        // Ops budget their results; this only catches a miscount.
        if json.len() > 3 * 1024 * 1024 {
            return Err(DbError::BadRequest("the result is too large".into()));
        }
        Ok(json)
    }) {
        Ok(json) => pb::DbResponse {
            request_id: req.request_id,
            json,
            ..Default::default()
        },
        Err(e) => {
            tracing::debug!(op = %req.op, deployment = %req.deployment_id, error = %e, "database operation failed");
            pb::DbResponse {
                request_id: req.request_id,
                bad_request: matches!(e, DbError::BadRequest(_)),
                error: e.to_string().chars().take(4000).collect(),
                ..Default::default()
            }
        }
    };
    let _ = tx
        .send(pb::NodeMessage {
            msg: Some(pb::node_message::Msg::DbResult(response)),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_parse_from_name_and_json() {
        assert_eq!(Op::parse("tables", "").unwrap(), Op::Tables);
        assert_eq!(Op::parse("stats", "null").unwrap(), Op::Stats);
        assert_eq!(
            Op::parse(
                "rows",
                r#"{"table":"t","page":2,"pageSize":50,"orderBy":"id","desc":true}"#
            )
            .unwrap(),
            Op::Rows(RowsArgs {
                table: "t".into(),
                page: 2,
                page_size: 50,
                order_by: Some("id".into()),
                desc: true,
            })
        );
        assert!(matches!(
            Op::parse("query", r#"{"sql":"select 1","extra":1}"#),
            Err(DbError::BadRequest(_))
        ));
        assert!(matches!(
            Op::parse("drop_everything", ""),
            Err(DbError::BadRequest(_))
        ));
        let p = Op::parse("password", r#"{"password":"hunter2hunter2"}"#).unwrap();
        assert!(!format!("{p:?}").contains("hunter2"));
        assert!(!p.is_read());
        assert!(Op::Tables.is_read() && Op::Stats.is_read() && Op::Extensions.is_read());
        assert_eq!(Op::parse("walStatus", "").unwrap(), Op::WalStatus);
        assert!(Op::WalStatus.is_read() && Op::Replication.is_read());
        assert!(!Op::Promote.is_read());
        let r = Op::parse("replicationRole", r#"{"password":"abcdefghijklmnop"}"#).unwrap();
        assert!(!r.is_read());
        assert_eq!(r.name(), "replicationRole");
        let q = |ro| Op::parse("query", &format!(r#"{{"sql":"select 1","readOnly":{ro}}}"#));
        assert!(q(true).unwrap().is_read());
        assert!(!q(false).unwrap().is_read());
        assert!(!Op::parse("vacuum", r#"{"table":"t"}"#).unwrap().is_read());
        assert!(
            !Op::parse("extension", r#"{"name":"x","install":true}"#)
                .unwrap()
                .is_read()
        );
    }

    #[test]
    fn credentials_default_like_the_postgres_image() {
        let env = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect()
        };
        assert_eq!(
            credentials(&env(&[("POSTGRES_PASSWORD", "pw")])).unwrap(),
            ("postgres".into(), "pw".into(), "postgres".into())
        );
        assert_eq!(
            credentials(&env(&[
                ("POSTGRES_USER", "app"),
                ("POSTGRES_PASSWORD", "pw"),
                ("POSTGRES_DB", "shop")
            ]))
            .unwrap(),
            ("app".into(), "pw".into(), "shop".into())
        );
        assert!(credentials(&env(&[("POSTGRES_USER", "app")])).is_err());
    }
}
