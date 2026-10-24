//! The real executor: tokio-postgres over the environment network (private,
//! so no TLS). Every user-supplied table or column name is first looked up in
//! the catalog with a bound parameter; only the names the catalog returned are
//! quoted into SQL, and values are always bound as text parameters cast to the
//! column's type.
//!
//! Reads connect as [`READER`], not the image's superuser. The node creates
//! that role in each database (through the owner's connection, once per
//! container) with `pg_read_all_data` and `pg_monitor`, read-only
//! transactions by default, and a password derived from a node-local
//! secret (`<data dir>/db-reader.secret`) and the service. Postgres before
//! 14 has no `pg_read_all_data`: there (or when the owner can't create
//! roles) reads keep using the owner, with a warning. The READ ONLY
//! transaction and keyword allowlist of the query editor stay regardless.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{StreamExt, pin_mut};
use serde_json::{Value, json};
use tokio_postgres::error::SqlState;
use tokio_postgres::types::{FromSql, ToSql, Type};
use tokio_postgres::{Client, NoTls, Row, SimpleQueryMessage};

use super::sql::{self, CELL_CHARS, qualified, quote_ident};
use super::{
    DbError, DeleteArgs, Executor, ExtensionArgs, InsertArgs, Op, QueryArgs, RESPONSE_BUDGET,
    RowsArgs, Target, UpdateArgs,
};

/// At most this many rows come back from the query editor.
pub const MAX_QUERY_ROWS: usize = 1000;
pub const MAX_PAGE_SIZE: u32 = 500;
const MAX_SQL_BYTES: usize = 1024 * 1024;
/// What a read-only query may start with. The READ ONLY transaction is the
/// real guard; this keeps out statements that could end or alter it (COMMIT,
/// ROLLBACK, BEGIN, SET, RESET, …).
const READ_ONLY_KEYWORDS: &[&str] = &["SELECT", "WITH", "VALUES", "TABLE", "EXPLAIN", "SHOW"];

/// Schemas the user owns: not the catalogs, TOAST or temp schemas.
macro_rules! user_schema {
    () => {
        "n.nspname NOT IN ('pg_catalog', 'information_schema') AND n.nspname NOT LIKE 'pg\\_%'"
    };
}

/// The read-only role reads connect as.
pub const READER: &str = "modula_reader";
const SECRET_FILE: &str = "db-reader.secret";
/// `pg_read_all_data` is new in Postgres 14.
const READER_MIN_VERSION: i64 = 140_000;

/// What this node knows about a container's reader role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderState {
    Ready,
    /// Too old, or the owner can't set it up: reads use the owner.
    Unsupported,
}

pub struct Postgres {
    /// The node-local secret reader passwords derive from; `None`: reads
    /// use the owner.
    secret: Option<[u8; 32]>,
    /// Container id → its reader role's state.
    readers: Mutex<HashMap<String, ReaderState>>,
}

impl Postgres {
    /// Reads as [`READER`], with the secret in `data_dir` (created on first
    /// use). Without one, reads use the owner (and a warning says why).
    pub fn new(data_dir: &Path) -> Self {
        let secret = match load_secret(&data_dir.join(SECRET_FILE)) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "no database reader secret: the Database tab reads as the database owner"
                );
                None
            }
        };
        Self {
            secret,
            readers: Mutex::default(),
        }
    }

    /// Every operation as the database owner (tests).
    pub fn owner_only() -> Self {
        Self {
            secret: None,
            readers: Mutex::default(),
        }
    }

    fn state(&self, container: &str) -> Option<ReaderState> {
        self.readers
            .lock()
            .expect("readers lock")
            .get(container)
            .copied()
    }

    fn set_state(&self, container: &str, state: Option<ReaderState>) {
        let mut readers = self.readers.lock().expect("readers lock");
        match state {
            Some(s) => readers.insert(container.to_owned(), s),
            None => readers.remove(container),
        };
    }

    /// A connection for reads: as the reader, set up first if needed.
    async fn reader(&self, target: &Target, timeout: Duration) -> Result<Client, DbError> {
        let Some(secret) = &self.secret else {
            return connect(target, timeout).await;
        };
        let password = reader_password(secret, target);
        let as_reader = Target {
            user: READER.into(),
            password: password.clone(),
            ..target.clone()
        };
        for retry in [false, true] {
            match self.state(&target.container_id) {
                Some(ReaderState::Unsupported) => return connect(target, timeout).await,
                Some(ReaderState::Ready) => {}
                None => {
                    let owner = connect(target, timeout).await?;
                    match ensure_reader(&owner, &target.database, &password).await {
                        Ok(true) => self.set_state(&target.container_id, Some(ReaderState::Ready)),
                        outcome => {
                            let why = match outcome {
                                Err(e) => message(&e),
                                _ => "Postgres is older than 14".into(),
                            };
                            tracing::warn!(
                                container = %target.container_id,
                                reason = %why,
                                "cannot set up the read-only database role: the Database tab reads as the database owner"
                            );
                            self.set_state(&target.container_id, Some(ReaderState::Unsupported));
                            return Ok(owner);
                        }
                    }
                }
            }
            match connect_raw(&as_reader, timeout).await {
                Ok(c) => return Ok(c),
                // Recreated, or its role changed: set it up again, once.
                Err(e) if !retry && is_auth_failure(&e) => {
                    self.set_state(&target.container_id, None);
                }
                Err(e) => return Err(unreachable(&e)),
            }
        }
        unreachable!("the second attempt returns")
    }
}

#[async_trait]
impl Executor for Postgres {
    async fn run(&self, target: &Target, op: Op, timeout: Duration) -> Result<Value, DbError> {
        let client = if op.is_read() {
            self.reader(target, timeout).await?
        } else {
            connect(target, timeout).await?
        };
        run(&client, op).await
    }
}

/// The 32-byte secret in `path`, created (mode 0600) if missing.
fn load_secret(path: &Path) -> anyhow::Result<[u8; 32]> {
    use std::io::{Read, Write};
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    match opts.open(path) {
        Ok(mut f) => {
            let mut secret = [0u8; 32];
            getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
            let written = f.write_all(&secret).and_then(|()| f.sync_all());
            if let Err(e) = written {
                let _ = std::fs::remove_file(path);
                return Err(e.into());
            }
            Ok(secret)
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let mut f = std::fs::File::open(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = f.metadata()?.permissions().mode();
                anyhow::ensure!(
                    mode & 0o077 == 0,
                    "{} is readable by other users (mode {:o})",
                    path.display(),
                    mode & 0o777
                );
            }
            let mut secret = [0u8; 32];
            f.read_exact(&mut secret)?;
            Ok(secret)
        }
        Err(e) => Err(anyhow::Error::from(e).context(format!("cannot create {}", path.display()))),
    }
}

/// The reader's password for `target`'s service (stable across redeploys).
fn reader_password(secret: &[u8; 32], target: &Target) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let id = if target.service_id.is_empty() {
        &target.container_id
    } else {
        &target.service_id
    };
    let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(secret).expect("any key length");
    mac.update(b"modula-db-reader:");
    mac.update(id.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// The statements that create or refresh the reader role. The password
/// goes in as a SCRAM verifier, so it never reaches the server's logs.
fn reader_sql(database: &str, verifier: &str) -> String {
    format!(
        "DO $modula$ BEGIN
           CREATE ROLE {role};
         EXCEPTION WHEN duplicate_object THEN NULL;
         END $modula$;
         ALTER ROLE {role} WITH LOGIN INHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION
           NOBYPASSRLS CONNECTION LIMIT 10 PASSWORD {verifier};
         ALTER ROLE {role} SET default_transaction_read_only = on;
         GRANT pg_read_all_data, pg_monitor TO {role};
         GRANT CONNECT ON DATABASE {database} TO {role};",
        role = quote_ident(READER),
        verifier = sql::quote_literal(verifier),
        database = quote_ident(database),
    )
}

/// Creates or refreshes the reader role through the owner's connection;
/// `false` when the server is too old for it.
async fn ensure_reader(
    owner: &Client,
    database: &str,
    password: &str,
) -> Result<bool, tokio_postgres::Error> {
    let version: i64 = owner
        .query_one("SELECT current_setting('server_version_num')::int8", &[])
        .await?
        .get(0);
    if version < READER_MIN_VERSION {
        return Ok(false);
    }
    let verifier = postgres_protocol::password::scram_sha_256(password.as_bytes());
    owner
        .batch_execute(&reader_sql(database, &verifier))
        .await?;
    Ok(true)
}

fn is_auth_failure(e: &tokio_postgres::Error) -> bool {
    matches!(
        e.code(),
        Some(c) if *c == SqlState::INVALID_PASSWORD
            || *c == SqlState::INVALID_AUTHORIZATION_SPECIFICATION
    )
}

pub async fn run(client: &Client, op: Op) -> Result<Value, DbError> {
    match op {
        Op::Tables => tables(client).await,
        Op::Rows(a) => rows(client, a).await,
        Op::Query(a) => query(client, a).await,
        Op::Insert(a) => insert(client, a).await,
        Op::Update(a) => update(client, a).await,
        Op::Delete(a) => delete(client, a).await,
        Op::Stats => stats(client).await,
        Op::Vacuum(a) => {
            let t = Table::resolve(client, &a.table).await?;
            let stmt = format!("VACUUM (ANALYZE) {}", t.quoted());
            client.batch_execute(&stmt).await.map_err(pg)?;
            Ok(json!({ "ok": true }))
        }
        Op::Extensions => extensions(client).await,
        Op::Extension(a) => extension(client, a).await,
        Op::Password(a) => {
            let ok = (16..=128).contains(&a.password.len())
                && a.password.bytes().all(|b| b.is_ascii_alphanumeric());
            if !ok {
                return Err(DbError::BadRequest("invalid password".into()));
            }
            // A SCRAM verifier, so the password itself never reaches the
            // server (or its statement log).
            let verifier = postgres_protocol::password::scram_sha_256(a.password.as_bytes());
            let stmt = format!(
                "ALTER USER CURRENT_USER WITH PASSWORD {}",
                sql::quote_literal(&verifier)
            );
            client.batch_execute(&stmt).await.map_err(pg)?;
            Ok(json!({ "ok": true }))
        }
    }
}

async fn connect(t: &Target, timeout: Duration) -> Result<Client, DbError> {
    connect_raw(t, timeout).await.map_err(|e| unreachable(&e))
}

fn unreachable(e: &tokio_postgres::Error) -> DbError {
    DbError::Unavailable(format!("cannot connect to the database: {}", message(e)))
}

async fn connect_raw(t: &Target, timeout: Duration) -> Result<Client, tokio_postgres::Error> {
    let ms = timeout.as_millis();
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(t.addr.ip().to_string())
        .port(t.addr.port())
        .user(&t.user)
        .password(&t.password)
        .dbname(&t.database)
        .application_name("modula")
        .ssl_mode(tokio_postgres::config::SslMode::Disable)
        .connect_timeout(Duration::from_secs(10))
        .options(format!(
            "-c statement_timeout={ms} -c lock_timeout={ms} -c idle_in_transaction_session_timeout={ms}"
        ));
    let (client, conn) = cfg.connect(NoTls).await?;
    // Ends when the client is dropped (end of the request).
    tokio::spawn(async move {
        let _ = conn.await;
    });
    Ok(client)
}

/// Postgres's own message (with detail and hint), or the client error.
fn message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => {
            let mut m = db.message().to_owned();
            if let Some(d) = db.detail() {
                m.push_str("\nDETAIL: ");
                m.push_str(d);
            }
            if let Some(h) = db.hint() {
                m.push_str("\nHINT: ");
                m.push_str(h);
            }
            m
        }
        None => e.to_string(),
    }
}

/// A statement failed: the database's error is the request's fault; a broken
/// connection is not.
fn pg(e: tokio_postgres::Error) -> DbError {
    if e.as_db_error().is_some() {
        DbError::BadRequest(message(&e))
    } else if e.is_closed() {
        DbError::Unavailable(format!("the database connection closed: {e}"))
    } else {
        DbError::BadRequest(message(&e))
    }
}

fn get<'a, T: FromSql<'a>>(row: &'a Row, i: usize) -> Result<T, DbError> {
    row.try_get(i)
        .map_err(|e| DbError::Unavailable(format!("unexpected catalog data: {e}")))
}

fn bad(msg: impl Into<String>) -> DbError {
    DbError::BadRequest(msg.into())
}

// ------------------------------------------------------------------ tables

async fn tables(client: &Client) -> Result<Value, DbError> {
    let rows = client
        .query(
            concat!(
                "SELECT n.nspname::text, c.relname::text,
                        (CASE WHEN c.reltuples < 0 THEN coalesce(s.n_live_tup, 0)
                              ELSE c.reltuples END)::int8
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid
                 WHERE c.relkind IN ('r', 'p') AND NOT c.relispartition AND ",
                user_schema!(),
                " ORDER BY n.nspname, c.relname"
            ),
            &[],
        )
        .await
        .map_err(pg)?;
    let mut tables = Vec::with_capacity(rows.len());
    for r in &rows {
        tables.push(json!({
            "schema": get::<String>(r, 0)?,
            "name": get::<String>(r, 1)?,
            "rowEstimate": get::<i64>(r, 2)?,
        }));
    }
    Ok(json!({ "tables": tables }))
}

struct Column {
    name: String,
    /// `format_type` output: a valid, already-quoted type expression.
    ty: String,
    nullable: bool,
    primary_key: bool,
}

/// A user table, resolved through the catalog.
struct Table {
    schema: String,
    name: String,
    columns: Vec<Column>,
}

impl Table {
    async fn resolve(client: &Client, input: &str) -> Result<Self, DbError> {
        let (schema, name) = sql::split_table(input);
        // `schema.name` first, then the whole input as a bare name (public first).
        let found = client
            .query_opt(
                concat!(
                    "SELECT c.oid, n.nspname::text, c.relname::text
                     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                     WHERE c.relkind IN ('r', 'p') AND ",
                    user_schema!(),
                    " AND ((n.nspname = $1::text AND c.relname = $2::text) OR c.relname = $3::text)
                     ORDER BY (n.nspname = $1::text AND c.relname = $2::text) DESC NULLS LAST,
                              n.nspname = 'public' DESC, n.nspname
                     LIMIT 1"
                ),
                &[&schema, &name, &input],
            )
            .await
            .map_err(pg)?
            .ok_or_else(|| bad(format!("table {input:?} does not exist")))?;
        let oid: u32 = get(&found, 0)?;
        let columns = client
            .query(
                "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                        coalesce(a.attnum = ANY(i.indkey), false)
                 FROM pg_attribute a
                 LEFT JOIN pg_index i ON i.indrelid = a.attrelid AND i.indisprimary
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                &[&oid],
            )
            .await
            .map_err(pg)?
            .iter()
            .map(|r| {
                Ok(Column {
                    name: get(r, 0)?,
                    ty: get(r, 1)?,
                    nullable: get(r, 2)?,
                    primary_key: get(r, 3)?,
                })
            })
            .collect::<Result<_, DbError>>()?;
        Ok(Self {
            schema: get(&found, 1)?,
            name: get(&found, 2)?,
            columns,
        })
    }

    fn quoted(&self) -> String {
        qualified(&self.schema, &self.name)
    }

    fn column(&self, name: &str) -> Result<&Column, DbError> {
        self.columns
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| bad(format!("column {name:?} does not exist")))
    }

    fn primary_key(&self) -> Vec<&Column> {
        self.columns.iter().filter(|c| c.primary_key).collect()
    }
}

async fn rows(client: &Client, a: RowsArgs) -> Result<Value, DbError> {
    let t = Table::resolve(client, &a.table).await?;
    let page_size = a.page_size.clamp(1, MAX_PAGE_SIZE);
    let page = a.page.max(1);
    let offset = u64::from(page - 1) * u64::from(page_size);
    let select = t
        .columns
        .iter()
        .map(|c| format!("left({}::text, {CELL_CHARS})", quote_ident(&c.name)))
        .collect::<Vec<_>>()
        .join(", ");
    let dir = if a.desc { "DESC" } else { "ASC" };
    let order: Vec<String> = match &a.order_by {
        Some(col) => vec![format!("{} {dir}", quote_ident(&t.column(col)?.name))],
        // Stable pages: the primary key, when there is one.
        None => t
            .primary_key()
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect(),
    };
    let order = if order.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", order.join(", "))
    };
    let stmt = format!(
        "SELECT {select} FROM {}{order} LIMIT {page_size} OFFSET {offset}",
        t.quoted()
    );
    let found = client.query(&stmt, &[]).await.map_err(pg)?;
    let total: i64 = get(
        &client
            .query_one(&format!("SELECT count(*) FROM {}", t.quoted()), &[])
            .await
            .map_err(pg)?,
        0,
    )?;
    let (mut out, mut used, mut truncated) = (Vec::with_capacity(found.len()), 0, false);
    for r in &found {
        if used > RESPONSE_BUDGET {
            truncated = true;
            break;
        }
        let mut cells = Vec::with_capacity(t.columns.len());
        for i in 0..t.columns.len() {
            let (v, n) = sql::render_cell(get::<Option<&str>>(r, i)?);
            used += n;
            cells.push(v);
        }
        out.push(Value::Array(cells));
    }
    let columns: Vec<Value> = t
        .columns
        .iter()
        .map(|c| {
            json!({ "name": c.name, "type": c.ty, "nullable": c.nullable, "primaryKey": c.primary_key })
        })
        .collect();
    Ok(json!({
        "columns": columns,
        "rows": out,
        "total": total,
        "page": page,
        "pageSize": page_size,
        "truncated": truncated,
    }))
}

// ------------------------------------------------------------------ query editor

/// The rows of one statement's result.
#[derive(Default)]
struct ResultSet {
    columns: Vec<String>,
    rows: Vec<Value>,
    count: u64,
    truncated: bool,
    /// Which statement produced it (index into the split script).
    statement: usize,
}

async fn query(client: &Client, a: QueryArgs) -> Result<Value, DbError> {
    if a.sql.trim().is_empty() {
        return Err(bad("the query is empty"));
    }
    if a.sql.len() > MAX_SQL_BYTES {
        return Err(bad("the query is longer than 1 MB"));
    }
    let statements = sql::split_statements(&a.sql);
    let single = a.read_only || statements.len() <= 1;
    if a.read_only {
        if !READ_ONLY_KEYWORDS.contains(&sql::leading_keyword(&a.sql).as_str()) {
            return Err(bad(
                "a read-only query must be one SELECT, WITH, VALUES, TABLE, EXPLAIN or SHOW statement",
            ));
        }
        client
            .batch_execute("BEGIN TRANSACTION READ ONLY")
            .await
            .map_err(pg)?;
    }
    // Describing the statement gives the column types (the simple protocol
    // has none). It runs nothing, and Postgres refuses to describe more than
    // one statement: for read-only queries that is the guarantee that the
    // text is exactly one statement, so it can't COMMIT its way out of the
    // READ ONLY transaction.
    let described = match (single, client.prepare(&a.sql).await) {
        (false, _) => None,
        (true, Ok(stmt)) => Some(stmt),
        (true, Err(e)) if a.read_only => return Err(pg(e)),
        (true, Err(_)) => None,
    };
    let started = Instant::now();
    let stream = client.simple_query_raw(&a.sql).await.map_err(pg)?;
    pin_mut!(stream);
    // The last statement that returned rows wins; else the last statement.
    let mut current: Option<ResultSet> = None;
    let mut last_rows: Option<ResultSet> = None;
    let mut last: Option<(u64, usize)> = None;
    let (mut done, mut used) = (0usize, 0usize);
    while let Some(m) = stream.next().await {
        match m.map_err(pg)? {
            SimpleQueryMessage::RowDescription(cols) => {
                current = Some(ResultSet {
                    columns: cols.iter().map(|c| c.name().to_owned()).collect(),
                    statement: done,
                    ..Default::default()
                });
            }
            SimpleQueryMessage::Row(r) => {
                let Some(set) = current.as_mut() else {
                    continue;
                };
                if set.rows.len() >= MAX_QUERY_ROWS || used > RESPONSE_BUDGET {
                    set.truncated = true;
                    // Only one statement: stop reading; dropping the client
                    // ends the query on the server.
                    if single {
                        break;
                    }
                    continue;
                }
                let mut cells = Vec::with_capacity(r.len());
                for i in 0..r.len() {
                    let (v, n) = sql::render_cell(r.try_get(i).map_err(pg)?);
                    used += n;
                    cells.push(v);
                }
                set.rows.push(Value::Array(cells));
            }
            SimpleQueryMessage::CommandComplete(n) => {
                match current.take() {
                    Some(mut set) => {
                        set.count = n;
                        last_rows = Some(set);
                    }
                    None => last = Some((n, done)),
                }
                done += 1;
            }
            _ => {}
        }
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    // A single statement cut short never completed.
    if let Some(mut set) = current.take() {
        set.count = set.rows.len() as u64;
        last_rows = Some(set);
    }
    let (count, statement) = match (&last_rows, last) {
        (Some(set), _) => (set.count, set.statement),
        (None, Some(last)) => last,
        (None, None) => (0, statements.len().saturating_sub(1)),
    };
    let command = statements
        .get(statement)
        .map(|s| sql::leading_keyword(s))
        .unwrap_or_default();
    let set = last_rows.unwrap_or_default();
    // A query cut short leaves rows in flight; dropping the connection ends
    // it (and its transaction) without waiting for them.
    if a.read_only && !set.truncated {
        client.batch_execute("ROLLBACK").await.map_err(pg)?;
    }
    let types: Vec<String> = described
        .map(|stmt| {
            stmt.columns()
                .iter()
                .map(|c| c.type_().name().to_owned())
                .collect()
        })
        .unwrap_or_default();
    let columns: Vec<Value> = set
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| json!({ "name": name, "type": types.get(i).cloned().unwrap_or_default() }))
        .collect();
    Ok(json!({
        "columns": columns,
        "rows": set.rows,
        "rowCount": count,
        "command": command,
        "durationMs": duration_ms,
        "truncated": set.truncated,
    }))
}

// ------------------------------------------------------------------ row editing

/// Bind values as text and let the SQL cast them: `$n::<column type>`.
async fn execute_text(
    client: &Client,
    stmt: &str,
    params: &[Option<String>],
) -> Result<u64, DbError> {
    let prepared = client
        .prepare_typed(stmt, &vec![Type::TEXT; params.len()])
        .await
        .map_err(pg)?;
    let refs: Vec<&(dyn ToSql + Sync)> = params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
    client.execute(&prepared, &refs).await.map_err(pg)
}

/// `"pk" = $n::type AND …` for exactly the table's primary key.
fn key_clause(
    t: &Table,
    key: &BTreeMap<String, String>,
    params: &mut Vec<Option<String>>,
) -> Result<String, DbError> {
    let pk = t.primary_key();
    if pk.is_empty() {
        return Err(bad("rows can only be changed in tables with a primary key"));
    }
    let want: BTreeSet<&str> = pk.iter().map(|c| c.name.as_str()).collect();
    let got: BTreeSet<&str> = key.keys().map(String::as_str).collect();
    if want != got {
        return Err(bad(format!(
            "the key must be exactly the primary key: {}",
            want.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }
    let mut parts = Vec::with_capacity(pk.len());
    for c in pk {
        params.push(Some(key[&c.name].clone()));
        parts.push(format!(
            "{} = ${}::{}",
            quote_ident(&c.name),
            params.len(),
            c.ty
        ));
    }
    Ok(parts.join(" AND "))
}

async fn insert(client: &Client, a: InsertArgs) -> Result<Value, DbError> {
    let t = Table::resolve(client, &a.table).await?;
    let mut params = Vec::with_capacity(a.values.len());
    let stmt = if a.values.is_empty() {
        format!("INSERT INTO {} DEFAULT VALUES", t.quoted())
    } else {
        let (mut cols, mut vals) = (vec![], vec![]);
        for (name, value) in &a.values {
            let c = t.column(name)?;
            params.push(value.clone());
            cols.push(quote_ident(&c.name));
            vals.push(format!("${}::{}", params.len(), c.ty));
        }
        format!(
            "INSERT INTO {} ({}) VALUES ({})",
            t.quoted(),
            cols.join(", "),
            vals.join(", ")
        )
    };
    let n = execute_text(client, &stmt, &params).await?;
    Ok(json!({ "rowCount": n }))
}

async fn update(client: &Client, a: UpdateArgs) -> Result<Value, DbError> {
    let t = Table::resolve(client, &a.table).await?;
    if a.values.is_empty() {
        return Err(bad("nothing to update"));
    }
    let mut params = Vec::with_capacity(a.values.len() + a.key.len());
    let mut sets = Vec::with_capacity(a.values.len());
    for (name, value) in &a.values {
        let c = t.column(name)?;
        params.push(value.clone());
        sets.push(format!(
            "{} = ${}::{}",
            quote_ident(&c.name),
            params.len(),
            c.ty
        ));
    }
    let filter = key_clause(&t, &a.key, &mut params)?;
    let stmt = format!(
        "UPDATE {} SET {} WHERE {filter}",
        t.quoted(),
        sets.join(", ")
    );
    let n = execute_text(client, &stmt, &params).await?;
    Ok(json!({ "rowCount": n }))
}

async fn delete(client: &Client, a: DeleteArgs) -> Result<Value, DbError> {
    let t = Table::resolve(client, &a.table).await?;
    let mut params = Vec::with_capacity(a.key.len());
    let filter = key_clause(&t, &a.key, &mut params)?;
    let stmt = format!("DELETE FROM {} WHERE {filter}", t.quoted());
    let n = execute_text(client, &stmt, &params).await?;
    Ok(json!({ "rowCount": n }))
}

// ------------------------------------------------------------------ stats

async fn stats(client: &Client) -> Result<Value, DbError> {
    let c = client
        .query_one(
            "SELECT count(*),
                    current_setting('max_connections')::int8,
                    count(*) FILTER (WHERE state = 'active'),
                    count(*) FILTER (WHERE state = 'idle'),
                    count(*) FILTER (WHERE state LIKE 'idle in transaction%')
             FROM pg_stat_activity WHERE backend_type = 'client backend'",
            &[],
        )
        .await
        .map_err(pg)?;
    let connections = json!({
        "used": get::<i64>(&c, 0)?,
        "max": get::<i64>(&c, 1)?,
        "active": get::<i64>(&c, 2)?,
        "idle": get::<i64>(&c, 3)?,
        "idleInTransaction": get::<i64>(&c, 4)?,
    });
    let cache: Option<f64> = get(
        &client
            .query_one(
                "SELECT (sum(heap_blks_hit) / nullif(sum(heap_blks_hit) + sum(heap_blks_read), 0))::float8
                 FROM pg_statio_user_tables",
                &[],
            )
            .await
            .map_err(pg)?,
        0,
    )?;
    let s = client
        .query_one(
            concat!(
                "SELECT pg_database_size(current_database()),
                        coalesce((SELECT sum(pg_table_size(c.oid)) FROM pg_class c
                                  JOIN pg_namespace n ON n.oid = c.relnamespace
                                  WHERE c.relkind IN ('r', 'm') AND ",
                user_schema!(),
                "), 0)::int8,
                        coalesce((SELECT sum(pg_indexes_size(c.oid)) FROM pg_class c
                                  JOIN pg_namespace n ON n.oid = c.relnamespace
                                  WHERE c.relkind IN ('r', 'm') AND ",
                user_schema!(),
                "), 0)::int8,
                        coalesce((SELECT sum(pg_total_relation_size(c.oid)) FROM pg_class c
                                  JOIN pg_namespace n ON n.oid = c.relnamespace
                                  WHERE c.relkind IN ('r', 'm')
                                    AND n.nspname IN ('pg_catalog', 'information_schema')), 0)::int8"
            ),
            &[],
        )
        .await
        .map_err(pg)?;
    let (database, data, indexes, system): (i64, i64, i64, i64) =
        (get(&s, 0)?, get(&s, 1)?, get(&s, 2)?, get(&s, 3)?);
    // Needs superuser or pg_monitor (the reader has it).
    let wal: i64 = match client
        .query_one(
            "SELECT coalesce(sum(size), 0)::int8 FROM pg_ls_waldir()",
            &[],
        )
        .await
    {
        Ok(r) => get(&r, 0)?,
        Err(_) => 0,
    };
    let sizes = json!({
        "database": database,
        "tables": data,
        "indexes": indexes,
        "system": system,
        "other": (database - data - indexes - system).max(0),
        "wal": wal,
    });

    let mut tables = vec![];
    for r in client
        .query(
            concat!(
                "SELECT n.nspname::text, c.relname::text, coalesce(s.n_live_tup, 0),
                        pg_table_size(c.oid), pg_indexes_size(c.oid),
                        coalesce(s.seq_scan, 0), coalesce(s.idx_scan, 0), coalesce(s.n_dead_tup, 0),
                        to_json(greatest(s.last_vacuum, s.last_autovacuum)) #>> '{}',
                        age(c.relfrozenxid)::int8
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 LEFT JOIN pg_stat_user_tables s ON s.relid = c.oid
                 WHERE c.relkind = 'r' AND ",
                user_schema!(),
                " ORDER BY pg_total_relation_size(c.oid) DESC, n.nspname, c.relname LIMIT 100"
            ),
            &[],
        )
        .await
        .map_err(pg)?
    {
        let (live, dead): (i64, i64) = (get(&r, 2)?, get(&r, 7)?);
        let dead_pct = if live + dead > 0 {
            (dead as f64 * 1000.0 / (live + dead) as f64).round() / 10.0
        } else {
            0.0
        };
        tables.push(json!({
            "schema": get::<String>(&r, 0)?,
            "name": get::<String>(&r, 1)?,
            "rows": live,
            "dataBytes": get::<i64>(&r, 3)?,
            "indexBytes": get::<i64>(&r, 4)?,
            "seqScans": get::<i64>(&r, 5)?,
            "idxScans": get::<i64>(&r, 6)?,
            "deadRows": dead,
            "deadPct": dead_pct,
            "lastVacuum": get::<Option<String>>(&r, 8)?,
            "xidAge": get::<i64>(&r, 9)?,
        }));
    }

    let mut unused = vec![];
    for r in client
        .query(
            "SELECT s.schemaname::text, s.indexrelname::text, s.relname::text,
                    pg_relation_size(s.indexrelid), s.idx_scan
             FROM pg_stat_user_indexes s JOIN pg_index i ON i.indexrelid = s.indexrelid
             WHERE s.idx_scan = 0 AND NOT i.indisunique AND NOT i.indisprimary
             ORDER BY pg_relation_size(s.indexrelid) DESC LIMIT 50",
            &[],
        )
        .await
        .map_err(pg)?
    {
        unused.push(json!({
            "schema": get::<String>(&r, 0)?,
            "name": get::<String>(&r, 1)?,
            "table": get::<String>(&r, 2)?,
            "bytes": get::<i64>(&r, 3)?,
            "scans": get::<i64>(&r, 4)?,
        }));
    }

    let (statements, top) = top_queries(client).await?;
    Ok(json!({
        "connections": connections,
        "cacheHitRatio": cache,
        "sizes": sizes,
        "tables": tables,
        "unusedIndexes": unused,
        "pgStatStatements": statements,
        "topQueries": top,
    }))
}

/// The slowest queries by total time, when pg_stat_statements is installed
/// (and preloaded; otherwise reading the view fails and it counts as off).
async fn top_queries(client: &Client) -> Result<(bool, Vec<Value>), DbError> {
    let schema: Option<String> = client
        .query_opt(
            "SELECT n.nspname::text FROM pg_extension e
             JOIN pg_namespace n ON n.oid = e.extnamespace WHERE e.extname = 'pg_stat_statements'",
            &[],
        )
        .await
        .map_err(pg)?
        .map(|r| get(&r, 0))
        .transpose()?;
    let Some(schema) = schema else {
        return Ok((false, vec![]));
    };
    let stmt = format!(
        "SELECT left(query, 2000), calls, mean_exec_time, total_exec_time
         FROM {} WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
         ORDER BY total_exec_time DESC LIMIT 10",
        qualified(&schema, "pg_stat_statements")
    );
    let Ok(rows) = client.query(&stmt, &[]).await else {
        return Ok((false, vec![]));
    };
    let mut top = Vec::with_capacity(rows.len());
    for r in &rows {
        top.push(json!({
            "query": get::<Option<String>>(r, 0)?,
            "calls": get::<i64>(r, 1)?,
            "meanMs": get::<f64>(r, 2)?,
            "totalMs": get::<f64>(r, 3)?,
        }));
    }
    Ok((true, top))
}

// ------------------------------------------------------------------ extensions

async fn extensions(client: &Client) -> Result<Value, DbError> {
    let (mut installed, mut available) = (vec![], vec![]);
    for r in client
        .query(
            "SELECT name::text, coalesce(installed_version, default_version)::text,
                    coalesce(comment, '')::text, installed_version IS NOT NULL
             FROM pg_available_extensions ORDER BY name",
            &[],
        )
        .await
        .map_err(pg)?
    {
        let e = json!({
            "name": get::<String>(&r, 0)?,
            "version": get::<Option<String>>(&r, 1)?,
            "comment": get::<String>(&r, 2)?,
        });
        if get::<bool>(&r, 3)? {
            installed.push(e);
        } else {
            available.push(e);
        }
    }
    Ok(json!({ "installed": installed, "available": available }))
}

async fn extension(client: &Client, a: ExtensionArgs) -> Result<Value, DbError> {
    let name: String = client
        .query_opt(
            "SELECT name::text FROM pg_available_extensions WHERE name = $1::text",
            &[&a.name],
        )
        .await
        .map_err(pg)?
        .map(|r| get(&r, 0))
        .transpose()?
        .ok_or_else(|| bad(format!("extension {:?} is not available", a.name)))?;
    // No CASCADE: dropping one that other objects use fails with Postgres's
    // explanation instead of silently dropping them too.
    let stmt = if a.install {
        format!("CREATE EXTENSION IF NOT EXISTS {}", quote_ident(&name))
    } else {
        format!("DROP EXTENSION IF EXISTS {}", quote_ident(&name))
    };
    client.batch_execute(&stmt).await.map_err(pg)?;
    extensions(client).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(service: &str) -> Target {
        Target {
            container_id: "c1".into(),
            service_id: service.into(),
            addr: "127.0.0.1:5432".parse().unwrap(),
            user: "postgres".into(),
            password: "pw".into(),
            database: "postgres".into(),
        }
    }

    #[test]
    fn reader_role_sql_quotes_everything() {
        let sql = reader_sql(r#"we"ird db"#, "SCRAM-SHA-256$4096:a'b");
        assert!(sql.contains(r#"GRANT CONNECT ON DATABASE "we""ird db" TO "modula_reader";"#));
        assert!(sql.contains("PASSWORD 'SCRAM-SHA-256$4096:a''b'"));
        assert!(sql.contains("GRANT pg_read_all_data, pg_monitor TO \"modula_reader\";"));
        assert!(sql.contains("NOSUPERUSER") && sql.contains("NOBYPASSRLS"));
        assert!(sql.contains("default_transaction_read_only = on"));
    }

    #[test]
    fn reader_passwords_are_per_service_and_stable() {
        let (a, b) = ([1u8; 32], [2u8; 32]);
        let p = reader_password(&a, &target("s1"));
        assert_eq!(p.len(), 64);
        assert_eq!(p, reader_password(&a, &target("s1")));
        assert_ne!(p, reader_password(&a, &target("s2")));
        assert_ne!(p, reader_password(&b, &target("s1")));
    }

    #[test]
    fn reader_secret_is_created_once_and_private() {
        let dir = std::env::temp_dir().join(format!("modula-dbs-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(SECRET_FILE);
        let first = load_secret(&path).unwrap();
        assert_eq!(load_secret(&path).unwrap(), first);
        assert_ne!(first, [0u8; 32]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(load_secret(&path).is_err(), "a readable secret is refused");
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Against a real postgres:17 (published on a loopback port). Needs
    /// Docker; `cargo test -p modula-node -- --ignored reads_as_the_reader`.
    #[tokio::test]
    #[ignore]
    async fn reads_as_the_reader_role() {
        use std::process::Command;
        let name = format!("modula-pg-test-{}", modula_id::Id::new());
        let ok = Command::new("docker")
            .args([
                "run",
                "-d",
                "--rm",
                "--name",
                &name,
                "-e",
                "POSTGRES_PASSWORD=owner-pw",
                "-p",
                "127.0.0.1::5432",
                "postgres:17",
            ])
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let port = String::from_utf8(
            Command::new("docker")
                .args(["port", &name, "5432/tcp"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let port: u16 = port.trim().rsplit(':').next().unwrap().parse().unwrap();
        let mut t = target("svc");
        t.addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        t.password = "owner-pw".into();
        let dir = std::env::temp_dir().join(format!("modula-dbr-{}", modula_id::Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Postgres::new(&dir);
        let timeout = Duration::from_secs(10);
        let mut ready = false;
        for _ in 0..60 {
            if db.run(&t, Op::Tables, timeout).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let q = |sql: &str, ro: bool| {
            Op::Query(super::super::QueryArgs {
                sql: sql.into(),
                read_only: ro,
            })
        };
        let outcome = async {
            assert!(ready, "postgres did not start");
            assert_eq!(db.state("c1"), Some(ReaderState::Ready));
            db.run(&t, q("CREATE TABLE t (id int primary key)", false), timeout)
                .await
                .unwrap();
            let who = db
                .run(&t, q("SELECT current_user", true), timeout)
                .await
                .unwrap();
            assert_eq!(who["rows"][0][0], READER, "{who}");
            // Reads work, writes don't, even outside the READ ONLY guard.
            db.run(&t, q("SELECT * FROM t", true), timeout)
                .await
                .unwrap();
            let reader = Target {
                user: READER.into(),
                password: reader_password(db.secret.as_ref().unwrap(), &t),
                ..t.clone()
            };
            let c = connect(&reader, timeout).await.unwrap();
            assert!(
                c.batch_execute(
                    "SET default_transaction_read_only = off; INSERT INTO t VALUES (1)"
                )
                .await
                .is_err()
            );
            assert!(c.batch_execute("CREATE ROLE x SUPERUSER").await.is_err());
            let stats = db.run(&t, Op::Stats, timeout).await.unwrap();
            assert!(
                stats["sizes"]["wal"].as_i64().unwrap() > 0,
                "pg_monitor: {stats}"
            );
            // A forgotten role is set up again.
            db.set_state("c1", None);
            db.run(&t, Op::Tables, timeout).await.unwrap();
        };
        let result = futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(outcome)).await;
        let _ = Command::new("docker").args(["rm", "-f", &name]).status();
        let _ = std::fs::remove_dir_all(dir);
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
