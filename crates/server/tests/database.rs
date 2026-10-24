//! The Database tab. End to end over real gRPC + mTLS with a fake executor on
//! the node (canned answers, recorded calls), plus an ignored test that runs
//! every operation against a real postgres:17 container started with the
//! template's TLS start command.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_node::db::fake::FakeDb;
use modula_node::db::{DbError, Executor, Op, Target};
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct Cluster {
    app: TestApp,
    admin: String,
    server_url: String,
    stop: CancellationToken,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

impl Cluster {
    async fn new(pool: PgPool) -> Self {
        let mut cfg = config();
        cfg.tcp_proxy_host = Some("db.example.test".into());
        let app = TestApp::with_config(pool, cfg);
        let admin = app.setup_admin().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = CancellationToken::new();
        tokio::spawn(modula_server::nodes::serve(
            app.state.clone(),
            listener,
            stop.clone(),
        ));
        Self {
            app,
            admin,
            server_url: format!("https://localhost:{port}"),
            stop,
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        self.app.call(method, path, Some(&self.admin), body).await
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let r = self.call(method.clone(), path, body).await;
        assert_eq!(r.status, StatusCode::OK, "{method} {path}: {:?}", r.json);
        r.json
    }

    async fn start_node(&self, db: Arc<FakeDb>) -> Arc<FakeRuntime> {
        let t = self.ok(Method::POST, "/api/nodes/join-tokens", None).await;
        let dir = std::env::temp_dir().join(format!("modula-node-test-{}", Id::new()));
        let id = join(JoinArgs {
            server: &self.server_url,
            token: t["token"].as_str().unwrap(),
            fingerprint: t["fingerprint"].as_str().unwrap(),
            hostname: "db-1".into(),
            dir: &dir,
        })
        .await
        .expect("join");
        let rt = Arc::new(FakeRuntime::new());
        let caps = modula_node::reconcile::Capabilities {
            db: Some(db),
            ..Default::default()
        };
        tokio::spawn(modula_node::session::run(
            dir,
            rt.clone(),
            caps,
            self.stop.child_token(),
        ));
        let node = id.meta.node_id.to_string();
        wait_for("node online", || async {
            let r = self.call(Method::GET, "/api/nodes", None).await;
            r.json
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["id"] == node && n["connected"] == true)
        })
        .await;
        rt
    }

    async fn wait_active(&self, deployment: &str) {
        wait_for("deployment active", || async {
            let r = self
                .call(Method::GET, &format!("/api/deployments/{deployment}"), None)
                .await;
            r.json["status"] == "active"
        })
        .await;
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn database_tab_runs_on_the_node(pool: PgPool) {
    let c = Cluster::new(pool).await;
    let db = Arc::new(FakeDb::default());
    let rt = c.start_node(db.clone()).await;
    let ws = c.app.create_workspace(&c.admin, "team").await;
    let project = c
        .ok(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(json!({"name": "shop"})),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let services = format!("/api/projects/{project}/services");
    let pg = c
        .ok(
            Method::POST,
            &services,
            Some(json!({ "name": "postgres", "template": "postgres" })),
        )
        .await;
    let pg = pg["instances"][0]["id"].as_str().unwrap().to_owned();
    let base = format!("/api/service-instances/{pg}/database");

    // Not deployed yet: a clear conflict, and no request reaches the node.
    let r = c.call(Method::GET, &format!("{base}/tables"), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);

    // A non-Postgres service has no Database tab.
    let web = c
        .ok(
            Method::POST,
            &services,
            Some(json!({ "name": "web", "config": { "source": { "type": "image", "image": "app:1" }, "port": 8080 } })),
        )
        .await;
    let web = web["instances"][0]["id"].as_str().unwrap().to_owned();
    let r = c
        .call(
            Method::GET,
            &format!("/api/service-instances/{web}/database/tables"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    // An app that uses the database (pending changes follow the password).
    c.ok(
        Method::PATCH,
        &format!("/api/service-instances/{web}/variables"),
        Some(json!({ "set": { "DATABASE_URL": "${{postgres.DATABASE_URL}}" } })),
    )
    .await;

    c.ok(
        Method::POST,
        &format!("/api/service-instances/{pg}/tcp-proxy"),
        Some(json!({ "targetPort": 5432 })),
    )
    .await;
    let deployment = c
        .ok(
            Method::POST,
            &format!("/api/service-instances/{pg}/deployments"),
            None,
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    c.wait_active(&deployment).await;
    let password = rt.env_of(&deployment)["POSTGRES_PASSWORD"].clone();
    let web_deployment = c
        .ok(
            Method::POST,
            &format!("/api/service-instances/{web}/deployments"),
            None,
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    c.wait_active(&web_deployment).await;
    let env: String = sqlx::query_scalar(
        "SELECT environment_id::text FROM service_instances WHERE id::text = $1",
    )
    .bind(&web)
    .fetch_one(c.app.db())
    .await
    .unwrap();
    let pending = format!("/api/environments/{env}/pending");
    let pending_services = || async {
        let p = c.ok(Method::GET, &pending, None).await;
        p["services"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["serviceName"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert!(!pending_services().await.contains(&"web".to_owned()));

    // Reads pass the node's JSON through; the node connects with the
    // deployment's credentials at the container's address.
    let tables = json!({ "tables": [{ "schema": "public", "name": "orders", "rowEstimate": 3 }] });
    db.answer("tables", Ok(tables.clone()));
    assert_eq!(
        c.ok(Method::GET, &format!("{base}/tables"), None).await,
        tables
    );
    let (target, op) = db.calls().pop().unwrap();
    assert_eq!(op, Op::Tables);
    assert_eq!(
        (
            target.user.as_str(),
            target.database.as_str(),
            target.addr.port()
        ),
        ("postgres", "app", 5432)
    );
    assert_eq!(target.password, password);
    assert!(target.addr.ip().to_string().starts_with("10.88.0."));

    let rows = json!({
        "columns": [{ "name": "id", "type": "integer", "nullable": false, "primaryKey": true }],
        "rows": [["1"], [null]], "total": 2, "page": 2, "pageSize": 10, "truncated": false,
    });
    db.answer("rows", Ok(rows.clone()));
    let got = c
        .ok(
            Method::GET,
            &format!("{base}/rows?table=public.orders&page=2&pageSize=10&orderBy=id&dir=desc"),
            None,
        )
        .await;
    assert_eq!(got, rows);
    let Op::Rows(a) = db.calls().pop().unwrap().1 else {
        panic!("expected a rows call");
    };
    assert_eq!(
        (
            a.table.as_str(),
            a.page,
            a.page_size,
            a.order_by.as_deref(),
            a.desc
        ),
        ("public.orders", 2, 10, Some("id"), true)
    );
    for bad in ["pageSize=501", "dir=up"] {
        let r = c
            .call(Method::GET, &format!("{base}/rows?table=t&{bad}"), None)
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad}");
    }

    // Postgres errors are the request's fault: 400 with Postgres's message.
    db.answer(
        "query",
        Err(DbError::BadRequest(
            "relation \"nope\" does not exist".into(),
        )),
    );
    let long_sql = format!("SELECT * FROM nope -- {}", "x".repeat(600));
    let r = c
        .call(
            Method::POST,
            &format!("{base}/query"),
            Some(json!({ "sql": long_sql })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        r.json["error"]["message"],
        "relation \"nope\" does not exist"
    );
    // The node being unable to reach the database is a conflict, not a 400.
    db.answer(
        "stats",
        Err(DbError::Unavailable(
            "cannot connect to the database".into(),
        )),
    );
    let r = c.call(Method::GET, &format!("{base}/stats"), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Writes are audited (SQL capped at 500 characters, values never).
    db.answer("insert", Ok(json!({ "rowCount": 1 })));
    let got = c
        .ok(
            Method::POST,
            &format!("{base}/rows"),
            Some(json!({ "table": "orders", "values": { "item": "secret-value", "note": null } })),
        )
        .await;
    assert_eq!(got, json!({ "rowCount": 1 }));
    db.answer("query", Ok(json!({ "rows": [] })));
    c.ok(
        Method::POST,
        &format!("{base}/query"),
        Some(json!({ "sql": "SELECT 1", "readOnly": true })),
    )
    .await;
    let Op::Query(q) = db.calls().pop().unwrap().1 else {
        panic!("expected a query call");
    };
    assert!(q.read_only);
    let r = c
        .call(
            Method::POST,
            &format!("{base}/extensions"),
            Some(json!({ "name": "pg_trgm", "action": "explode" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, metadata FROM audit_log WHERE action LIKE 'database.%' ORDER BY id",
    )
    .fetch_all(c.app.db())
    .await
    .unwrap();
    let actions: Vec<&str> = audit.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        ["database.query", "database.row.insert", "database.query"]
    );
    assert_eq!(audit[2].1["readOnly"], true);
    assert_eq!(audit[0].1["sql"].as_str().unwrap().chars().count(), 500);
    assert!(!audit[1].1.to_string().contains("secret-value"));

    // Credentials come from the instance's variables; public access needs TLS.
    let creds = c
        .ok(Method::GET, &format!("{base}/credentials"), None)
        .await;
    assert_eq!(creds["template"], "postgres");
    assert_eq!(creds["username"], "postgres");
    assert_eq!(creds["password"], password.as_str());
    assert_eq!(creds["host"], "postgres.modula.internal");
    assert_eq!(
        creds["privateUrl"],
        format!("postgresql://postgres:{password}@postgres.modula.internal:5432/app")
    );
    assert_eq!(
        creds["publicUrl"],
        format!("postgresql://postgres:{password}@db.example.test:20000/app?sslmode=require")
    );

    // Rotating the password: changed on the live database first, then stored.
    let r = c.ok(Method::POST, &format!("{base}/password"), None).await;
    let new = r["password"].as_str().unwrap().to_owned();
    assert_eq!(new.len(), 32);
    assert_ne!(new, password);
    let Op::Password(p) = db.calls().pop().unwrap().1 else {
        panic!("expected a password call");
    };
    assert_eq!(p.password, new);
    assert!(r["privateUrl"].as_str().unwrap().contains(&new));
    // The node picks the new password up from desired state, without a restart.
    db.answer("tables", Ok(tables.clone()));
    wait_for("the node to use the new password", || async {
        c.ok(Method::GET, &format!("{base}/tables"), None).await;
        db.calls().pop().unwrap().0.password == new
    })
    .await;
    assert!(rt.running_deployments().contains(&deployment));
    // The app referencing the database has a pending change to deploy.
    let pending = pending_services().await;
    assert!(pending.contains(&"web".to_owned()));
    assert!(
        !pending.iter().any(|s| s != "web"),
        "the database's own password is part of what's deployed: {pending:?}"
    );
    let audit: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM audit_log WHERE action LIKE 'database.%' ORDER BY id",
    )
    .fetch_all(c.app.db())
    .await
    .unwrap();
    assert!(audit.contains(&"database.credentials.read".to_owned()));
    assert!(audit.contains(&"database.password.regenerate".to_owned()));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn credentials_for_every_database_template(pool: PgPool) {
    let c = Cluster::new(pool).await;
    let ws = c.app.create_workspace(&c.admin, "team").await;
    let project = c
        .ok(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(json!({"name": "shop"})),
        )
        .await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // (template, user, database, port, URL scheme)
    let cases = [
        ("postgres", "postgres", Some("app"), 5432, "postgresql"),
        ("mysql", "root", Some("app"), 3306, "mysql"),
        ("redis", "default", None, 6379, "redis"),
        ("mongo", "mongo", None, 27017, "mongodb"),
    ];
    for (template, user, database, port, scheme) in cases {
        let r = c
            .ok(
                Method::POST,
                &format!("/api/projects/{project}/services"),
                Some(json!({ "name": template, "template": template })),
            )
            .await;
        let inst = r["instances"][0]["id"].as_str().unwrap().to_owned();
        // Credentials don't need a running deployment (or a server).
        let creds = c
            .ok(
                Method::GET,
                &format!("/api/service-instances/{inst}/database/credentials"),
                None,
            )
            .await;
        let password = creds["password"].as_str().unwrap();
        assert_eq!(password.len(), 32, "{template}");
        assert_eq!(creds["template"], template);
        assert_eq!(creds["username"], user, "{template}");
        assert_eq!(creds["database"], json!(database), "{template}");
        assert_eq!(creds["host"], format!("{template}.modula.internal"));
        assert_eq!(creds["port"], port);
        let path = database.map(|d| format!("/{d}")).unwrap_or_default();
        assert_eq!(
            creds["privateUrl"],
            format!("{scheme}://{user}:{password}@{template}.modula.internal:{port}{path}"),
            "{template}"
        );
        assert_eq!(creds["publicUrl"], Value::Null, "{template}");
        // The rest of the Database tab is Postgres-only.
        if template != "postgres" {
            let r = c
                .call(
                    Method::GET,
                    &format!("/api/service-instances/{inst}/database/tables"),
                    None,
                )
                .await;
            assert_eq!(r.status, StatusCode::CONFLICT, "{template}");
        }
    }
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'database.credentials.read'",
    )
    .fetch_one(c.app.db())
    .await
    .unwrap();
    assert_eq!(audited, 4);
}

// ---------------------------------------------------------------- real Postgres

fn docker(args: &[&str]) -> String {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .expect("docker");
    assert!(
        out.status.success(),
        "docker {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

const PASSWORD: &str = "Pw0123456789abcdefABCDEF01234567";

/// psql inside the container over TCP, TLS required.
fn psql(container: &str, password: &str, sql: &str) -> std::process::Output {
    std::process::Command::new("docker")
        .args([
            "exec",
            "-e",
            &format!("PGPASSWORD={password}"),
            container,
            "psql",
            "host=127.0.0.1 user=postgres dbname=app sslmode=require",
            "-v",
            "ON_ERROR_STOP=1",
            "-At",
            "--command",
            sql,
        ])
        .output()
        .expect("docker")
}

async fn run(target: &Target, op: &str, args: Value) -> Result<Value, DbError> {
    let op = Op::parse(op, &args.to_string()).unwrap();
    modula_node::db::pg::Postgres::owner_only()
        .run(target, op, Duration::from_secs(30))
        .await
}

async fn ok(target: &Target, op: &str, args: Value) -> Value {
    run(target, op, args.clone())
        .await
        .unwrap_or_else(|e| panic!("{op} {args}: {e}"))
}

#[tokio::test]
#[ignore = "needs Docker (runs postgres:17)"]
async fn real_postgres_operations_and_template_tls() {
    let template = modula_server::templates::get("postgres").unwrap();
    let name = format!("modula-db-test-{}", Id::new());
    let start = template.start_command.unwrap();
    let mut args = vec![
        "run",
        "-d",
        "--rm",
        "--name",
        &name,
        "-e",
        "POSTGRES_USER=postgres",
        "-e",
        "POSTGRES_DB=app",
        "-e",
        "PGDATA=/var/lib/postgresql/data/pgdata",
        "-p",
        "127.0.0.1::5432",
    ];
    let pw_env = format!("POSTGRES_PASSWORD={PASSWORD}");
    args.extend(["-e", &pw_env, template.image]);
    args.extend(start.iter().copied());
    docker(&args);
    struct Remove(String);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::process::Command::new("docker")
                .args(["rm", "-f", &self.0])
                .output();
        }
    }
    let _remove = Remove(name.clone());
    // The image restarts the server once after initdb; wait for the real one.
    let mut ready = false;
    for _ in 0..120 {
        if psql(&name, PASSWORD, "SELECT 1").status.success() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        ready,
        "postgres did not start: {}",
        docker(&["logs", &name])
    );
    // TLS is on (the template's self-signed certificate).
    let ssl = psql(
        &name,
        PASSWORD,
        "SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
    );
    assert_eq!(String::from_utf8_lossy(&ssl.stdout).trim(), "t");

    let setup = psql(
        &name,
        PASSWORD,
        r#"CREATE TABLE orders (id int PRIMARY KEY, item text NOT NULL, qty int, big text,
                                created timestamptz DEFAULT now());
           INSERT INTO orders (id, item, qty) SELECT g, 'item ' || g, g FROM generate_series(1, 120) g;
           UPDATE orders SET big = repeat('x', 20000) WHERE id = 1;
           CREATE INDEX orders_qty ON orders (qty);
           CREATE TABLE log (msg text);
           CREATE SCHEMA "Sales";
           CREATE TABLE "Sales"."we""ird" ("k""ey" text PRIMARY KEY, v jsonb);
           INSERT INTO "Sales"."we""ird" VALUES ('a', '{"x": 1}');
           ANALYZE;"#,
    );
    assert!(
        setup.status.success(),
        "{}",
        String::from_utf8_lossy(&setup.stderr)
    );

    let port = docker(&["port", &name, "5432"]);
    let addr: SocketAddr = port.lines().next().unwrap().parse().unwrap();
    let mut t = Target {
        container_id: name.clone(),
        service_id: String::new(),
        addr,
        user: "postgres".into(),
        password: PASSWORD.into(),
        database: "app".into(),
    };

    // Tables: user schemas only, ordered.
    let tables = ok(&t, "tables", Value::Null).await;
    let names: Vec<String> = tables["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            format!(
                "{}.{}",
                t["schema"].as_str().unwrap(),
                t["name"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(names, ["Sales.we\"ird", "public.log", "public.orders"]);
    assert_eq!(tables["tables"][2]["rowEstimate"], 120);

    // Rows: paging, ordering, capped cells, catalog-checked names.
    let page = ok(
        &t,
        "rows",
        json!({ "table": "orders", "page": 1, "pageSize": 50, "orderBy": "id", "desc": false }),
    )
    .await;
    assert_eq!(page["total"], 120);
    assert_eq!(page["rows"].as_array().unwrap().len(), 50);
    assert_eq!(
        page["columns"][0],
        json!({ "name": "id", "type": "integer", "nullable": false, "primaryKey": true })
    );
    assert_eq!(page["columns"][2]["nullable"], true);
    let big = page["rows"][0][3].as_str().unwrap();
    assert!(big.ends_with('…') && big.len() < 10_300, "{}", big.len());
    let last = ok(
        &t,
        "rows",
        json!({ "table": "public.orders", "page": 3, "pageSize": 50, "orderBy": "qty", "desc": true }),
    )
    .await;
    assert_eq!(last["rows"].as_array().unwrap().len(), 20);
    assert_eq!(last["rows"][19][0], "1");
    let weird = ok(
        &t,
        "rows",
        json!({ "table": "Sales.we\"ird", "page": 1, "pageSize": 10 }),
    )
    .await;
    assert_eq!(weird["rows"], json!([["a", "{\"x\": 1}"]]));
    for bad in [
        json!({ "table": "orders; DROP TABLE orders", "page": 1, "pageSize": 10 }),
        json!({ "table": "orders", "page": 1, "pageSize": 10, "orderBy": "id; DROP TABLE orders" }),
        json!({ "table": "pg_catalog.pg_authid", "page": 1, "pageSize": 10 }),
    ] {
        assert!(
            matches!(
                run(&t, "rows", bad.clone()).await,
                Err(DbError::BadRequest(_))
            ),
            "{bad}"
        );
    }

    // The query editor: last result set, types, capped rows, PG errors.
    let q = ok(
        &t,
        "query",
        json!({ "sql": "SELECT id, item FROM orders WHERE id <= 3 ORDER BY id" }),
    )
    .await;
    assert_eq!(q["command"], "SELECT");
    assert_eq!(q["rowCount"], 3);
    assert_eq!(
        q["columns"],
        json!([{ "name": "id", "type": "int4" }, { "name": "item", "type": "text" }])
    );
    assert_eq!(q["rows"][2], json!(["3", "item 3"]));
    let q = ok(
        &t,
        "query",
        json!({ "sql": "SELECT g FROM generate_series(1, 5000) g" }),
    )
    .await;
    assert_eq!(q["rows"].as_array().unwrap().len(), 1000);
    assert_eq!(q["truncated"], true);
    let q = ok(
        &t,
        "query",
        json!({ "sql": "INSERT INTO log VALUES ('a;b'); SELECT count(*) AS n FROM log; INSERT INTO log VALUES ('c')" }),
    )
    .await;
    assert_eq!(q["command"], "SELECT");
    assert_eq!(q["rows"], json!([["1"]]));
    let q = ok(
        &t,
        "query",
        json!({ "sql": "UPDATE orders SET qty = qty WHERE id < 10" }),
    )
    .await;
    assert_eq!(
        (q["command"].as_str(), q["rowCount"].as_u64()),
        (Some("UPDATE"), Some(9))
    );
    // Read-only: one statement in a READ ONLY transaction, nothing escapes.
    let q = ok(
        &t,
        "query",
        json!({ "sql": "SELECT count(*) AS n FROM orders", "readOnly": true }),
    )
    .await;
    assert_eq!(q["rows"], json!([["120"]]));
    assert_eq!(q["columns"], json!([{ "name": "n", "type": "int8" }]));
    let q = ok(
        &t,
        "query",
        json!({ "sql": "SELECT g FROM generate_series(1, 5000) g", "readOnly": true }),
    )
    .await;
    assert_eq!(q["truncated"], true);
    for sql in [
        "SELECT 1; DELETE FROM orders",
        "SELECT 1; COMMIT; DELETE FROM orders",
        "COMMIT",
        "DELETE FROM orders",
        "SET transaction_read_only = off",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
        "WITH d AS (DELETE FROM orders RETURNING *) SELECT count(*) FROM d",
        "SELECT set_config('transaction_read_only', 'off', false)",
        "EXPLAIN ANALYZE DELETE FROM orders",
    ] {
        let r = run(&t, "query", json!({ "sql": sql, "readOnly": true })).await;
        assert!(matches!(r, Err(DbError::BadRequest(_))), "{sql}: {r:?}");
    }
    let q = ok(&t, "query", json!({ "sql": "SELECT count(*) FROM orders" })).await;
    assert_eq!(q["rows"], json!([["120"]]));

    match run(&t, "query", json!({ "sql": "SELECT * FROM nope" })).await {
        Err(DbError::BadRequest(m)) => assert!(m.contains("\"nope\" does not exist"), "{m}"),
        other => panic!("{other:?}"),
    }

    // Row editing: text values cast to the column type, by primary key.
    let r = ok(
        &t,
        "insert",
        json!({ "table": "orders", "values": { "id": "500", "item": "new", "qty": null } }),
    )
    .await;
    assert_eq!(r["rowCount"], 1);
    let r = ok(&t, "update", json!({ "table": "orders", "key": { "id": "500" }, "values": { "qty": "7", "item": "it's" } })).await;
    assert_eq!(r["rowCount"], 1);
    let q = ok(
        &t,
        "query",
        json!({ "sql": "SELECT item, qty FROM orders WHERE id = 500" }),
    )
    .await;
    assert_eq!(q["rows"], json!([["it's", "7"]]));
    assert!(matches!(
        run(
            &t,
            "update",
            json!({ "table": "orders", "key": { "id": "500" }, "values": { "qty": "seven" } })
        )
        .await,
        Err(DbError::BadRequest(_))
    ));
    assert!(matches!(
        run(
            &t,
            "delete",
            json!({ "table": "log", "key": { "msg": "a;b" } })
        )
        .await,
        Err(DbError::BadRequest(_))
    ));
    assert!(matches!(
        run(
            &t,
            "delete",
            json!({ "table": "orders", "key": { "item": "new" } })
        )
        .await,
        Err(DbError::BadRequest(_))
    ));
    let r = ok(
        &t,
        "delete",
        json!({ "table": "Sales.we\"ird", "key": { "k\"ey": "a" } }),
    )
    .await;
    assert_eq!(r["rowCount"], 1);
    let r = ok(
        &t,
        "delete",
        json!({ "table": "orders", "key": { "id": "500" } }),
    )
    .await;
    assert_eq!(r["rowCount"], 1);

    // Maintenance and extensions.
    ok(&t, "vacuum", json!({ "table": "orders" })).await;
    let ext = ok(&t, "extensions", Value::Null).await;
    assert!(
        ext["installed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "plpgsql")
    );
    assert!(
        ext["available"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "pg_stat_statements")
    );
    let ext = ok(
        &t,
        "extension",
        json!({ "name": "pg_stat_statements", "install": true }),
    )
    .await;
    assert!(
        ext["installed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["name"] == "pg_stat_statements")
    );
    assert!(matches!(
        run(
            &t,
            "extension",
            json!({ "name": "no_such_ext", "install": true })
        )
        .await,
        Err(DbError::BadRequest(_))
    ));

    let s = ok(&t, "stats", Value::Null).await;
    assert!(s["connections"]["used"].as_i64().unwrap() >= 1);
    assert_eq!(s["connections"]["max"], 100);
    assert!(s["sizes"]["database"].as_i64().unwrap() > s["sizes"]["tables"].as_i64().unwrap());
    assert!(s["sizes"]["wal"].as_i64().unwrap() > 0);
    let orders = s["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "orders")
        .unwrap();
    assert!(orders["dataBytes"].as_i64().unwrap() > 0);
    assert!(orders["lastVacuum"].is_string(), "{orders}");
    assert!(
        s["unusedIndexes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["name"] == "orders_qty")
    );
    assert_eq!(s["pgStatStatements"], true, "{s}");
    assert!(s["topQueries"].as_array().is_some());
    ok(
        &t,
        "extension",
        json!({ "name": "pg_stat_statements", "install": false }),
    )
    .await;

    // Password rotation: the old one stops working, the new one works.
    let new = "Nw0123456789abcdefABCDEF01234567";
    ok(&t, "password", json!({ "password": new })).await;
    assert!(matches!(
        run(&t, "tables", Value::Null).await,
        Err(DbError::Unavailable(_))
    ));
    t.password = new.into();
    ok(&t, "tables", Value::Null).await;
    assert!(psql(&name, new, "SELECT 1").status.success());
    assert!(matches!(
        run(&t, "password", json!({ "password": "short" })).await,
        Err(DbError::BadRequest(_))
    ));
}
