//! The `modula-deploy` CLI end to end: its command functions (parsed from
//! real arguments) against the real HTTP server with an API token, and a node
//! on a fake runtime with a fake database executor.

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use clap::Parser;
use common::*;
use modula_cli::cli::Cli;
use modula_cli::commands;
use modula_cli::context::Session;
use modula_cli::output::{Output, UsageError};
use modula_id::Id;
use modula_node::db::fake::FakeDb;
use modula_node::db::{Op, QueryArgs};
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct Setup {
    app: TestApp,
    admin: String,
    api_url: String,
    token: String,
    /// The "project directory" the CLI runs in (and its config file).
    dir: PathBuf,
    db: Arc<FakeDb>,
    stop: CancellationToken,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.stop.cancel();
        let _ = std::fs::remove_dir_all(&self.dir);
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

/// HTTP + node servers, one node (fake runtime, fake database), a token.
async fn setup(pool: PgPool) -> Setup {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let stop = CancellationToken::new();

    let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_port = grpc.local_addr().unwrap().port();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        grpc,
        stop.clone(),
    ));
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}/", http.local_addr().unwrap());
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(
            http,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let t = app
        .call(Method::POST, "/api/nodes/join-tokens", Some(&admin), None)
        .await;
    let node_dir = std::env::temp_dir().join(format!("modula-cli-node-{}", Id::new()));
    let id = join(JoinArgs {
        server: &format!("https://localhost:{grpc_port}"),
        token: t.json["token"].as_str().unwrap(),
        fingerprint: t.json["fingerprint"].as_str().unwrap(),
        hostname: "cli-1".into(),
        dir: &node_dir,
    })
    .await
    .expect("join");
    let db = Arc::new(FakeDb::default());
    let caps = modula_node::reconcile::Capabilities {
        db: Some(db.clone()),
        ..Default::default()
    };
    tokio::spawn(modula_node::session::run(
        node_dir,
        Arc::new(FakeRuntime::new()),
        caps,
        stop.child_token(),
    ));
    let node = id.meta.node_id;
    let st = app.state.clone();
    wait_for("node online", || {
        let st = st.clone();
        async move { st.hub.is_connected(node) }
    })
    .await;

    let r = app
        .call(
            Method::POST,
            "/api/account/tokens",
            Some(&admin),
            Some(json!({ "name": "cli" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let dir = std::env::temp_dir().join(format!("modula-cli-project-{}", Id::new()));
    std::fs::create_dir_all(&dir).unwrap();
    Setup {
        token: r.json["token"].as_str().unwrap().to_owned(),
        app,
        admin,
        api_url,
        dir,
        db,
        stop,
    }
}

impl Setup {
    /// Run `modula-deploy <args>` in the project directory; returns the exit
    /// code (or error) and what it printed.
    async fn run(&self, args: &[&str]) -> (anyhow::Result<i32>, String) {
        let cli = Cli::try_parse_from(std::iter::once("modula-deploy").chain(args.iter().copied()))
            .expect("valid arguments");
        let (out, buf) = Output::capture(cli.global.json);
        let session = Session {
            config_path: self.dir.join("config.toml"),
            cwd: self.dir.clone(),
            env_url: Some(self.api_url.clone()),
            env_token: Some(self.token.clone()),
            interactive: false,
        };
        let r = commands::run(cli, session, out).await;
        let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        (r, text)
    }

    /// Run a command that must succeed; returns its output.
    async fn ok(&self, args: &[&str]) -> String {
        let (r, text) = self.run(args).await;
        match r {
            Ok(0) => text,
            Ok(code) => panic!("{args:?} exited {code}: {text}"),
            Err(e) => panic!("{args:?} failed: {e:#}\n{text}"),
        }
    }

    async fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let text = self.ok(&all).await;
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{args:?}: {e}: {text}"))
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn add_database_set_variables_deploy_status_and_query(pool: PgPool) {
    let s = setup(pool).await;
    s.app.create_workspace(&s.admin, "team").await;

    // init: creates the project and links the directory.
    let p = s.json(&["init", "shop", "--workspace", "team"]).await;
    assert_eq!(p["name"], "shop");
    let link = std::fs::read_to_string(s.dir.join(".modula/project.toml")).unwrap();
    assert!(link.contains("environment = \"production\""), "{link}");
    let listed = s.json(&["list"]).await;
    assert_eq!(listed[0]["name"], "shop");
    assert_eq!(listed[0]["linked"], true);

    // add --database postgres
    let created = s.json(&["add", "--database", "postgres"]).await;
    assert_eq!(created["name"], "postgres");
    // The only service is the default one.
    let before = s.json(&["status"]).await;
    assert_eq!(before["services"][0]["name"], "postgres");
    assert!(before["services"][0]["status"].is_null());

    // variables set: pending until deployed.
    let text = s
        .ok(&["variables", "set", "APP_MODE=demo", "URL=a=b"])
        .await;
    assert!(
        text.contains("Set APP_MODE, URL in postgres (shop/production)"),
        "{text}"
    );
    let vars = s.json(&["variables"]).await;
    let names: Vec<&str> = vars
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"APP_MODE") && names.contains(&"POSTGRES_PASSWORD"),
        "{names:?}"
    );
    // Values never come back; --kv masks them.
    assert!(
        vars.as_array()
            .unwrap()
            .iter()
            .all(|v| v.get("value").is_none()),
        "{vars}"
    );
    let kv = s.ok(&["variables", "--kv"]).await;
    assert!(kv.contains("APP_MODE=********"), "{kv}");
    let pending = s.json(&["status"]).await;
    assert!(
        pending["pendingChanges"].as_u64().unwrap() >= 1,
        "{pending}"
    );

    // deploy --wait: every pending service, until it is live.
    let deployed = s.json(&["deploy", "--wait"]).await;
    assert_eq!(
        deployed["deployments"].as_array().unwrap().len(),
        1,
        "{deployed}"
    );
    assert_eq!(deployed["settled"][0]["status"], "active", "{deployed}");

    // status --json
    let status = s.json(&["status"]).await;
    assert_eq!(status["project"]["name"], "shop");
    assert_eq!(status["environment"]["name"], "production");
    let svc = &status["services"][0];
    assert_eq!(svc["name"], "postgres");
    assert_eq!(svc["template"], "postgres");
    assert_eq!(svc["status"], "active");
    assert_eq!(svc["latestDeploymentId"], deployed["settled"][0]["id"]);
    assert_eq!(status["pendingChanges"], 0);
    let human = s.ok(&["status"]).await;
    assert!(human.contains("postgres  active"), "{human}");
    let list = s.json(&["deployment", "list"]).await;
    assert_eq!(list[0]["status"], "active");

    // db query: read-only unless --write.
    s.db.answer(
        "query",
        Ok(json!({
            "columns": [{ "name": "n", "type": "int4" }],
            "rows": [["1"]], "rowCount": 1, "command": "SELECT", "durationMs": 2, "truncated": false,
        })),
    );
    let table = s.ok(&["db", "query", "select 1 as n"]).await;
    assert_eq!(table, "n\n1\n1 row (SELECT, 2 ms)\n");
    let (_, op) = s.db.calls().pop().unwrap();
    assert_eq!(
        op,
        Op::Query(QueryArgs {
            sql: "select 1 as n".into(),
            read_only: true
        })
    );
    let raw = s.json(&["db", "query", "select 1 as n"]).await;
    assert_eq!(raw["rows"][0][0], "1");

    // Writes must be confirmed; without a terminal that means --yes (exit 2).
    let calls = s.db.calls().len();
    let (r, _) = s.run(&["db", "query", "delete from t", "--write"]).await;
    let e = r.unwrap_err();
    assert!(e.downcast_ref::<UsageError>().is_some(), "{e:#}");
    assert_eq!(s.db.calls().len(), calls, "nothing ran");
    s.ok(&["db", "query", "delete from t", "--write", "--yes"])
        .await;
    let (_, op) = s.db.calls().pop().unwrap();
    assert!(matches!(
        op,
        Op::Query(QueryArgs {
            read_only: false,
            ..
        })
    ));

    s.db.answer(
        "tables",
        Ok(json!({ "tables": [{ "schema": "public", "name": "orders", "rowEstimate": 3 }] })),
    );
    let tables = s.ok(&["db", "tables"]).await;
    assert!(tables.contains("public  orders  3"), "{tables}");

    // Environments: create one copying production, then the link follows.
    s.ok(&["environment", "new", "staging", "--copy", "production"])
        .await;
    let status = s.json(&["status"]).await;
    assert_eq!(status["environment"]["name"], "staging");
    assert!(status["services"][0]["status"].is_null());
    s.ok(&["environment", "production"]).await;
    let envs = s.json(&["environment"]).await;
    assert_eq!(envs.as_array().unwrap().len(), 2);

    // --deploy --json prints one document: the deploy result.
    let redeployed = s
        .json(&["variables", "set", "APP_MODE=live", "--deploy"])
        .await;
    assert_eq!(
        redeployed["deployments"].as_array().unwrap().len(),
        1,
        "{redeployed}"
    );
    let d = redeployed["deployments"][0]["deploymentId"]
        .as_str()
        .unwrap()
        .to_owned();
    let st = s.app.state.clone();
    wait_for("redeploy active", || {
        let st = st.clone();
        let d = d.clone();
        async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM deployments WHERE id::text = $1")
                .bind(&d)
                .fetch_one(&st.db)
                .await
                .is_ok_and(|s| s == "active")
        }
    })
    .await;

    // A generated domain routed to a port, then listed.
    let d = s.json(&["domain", "--port", "5432"]).await;
    assert!(
        d["hostname"].as_str().unwrap().ends_with(".apps.test"),
        "{d}"
    );
    assert_eq!(d["targetPort"], 5432);
    let domains = s.ok(&["domain", "list"]).await;
    assert!(domains.contains("generated  verified  5432"), "{domains}");

    // down: confirmed, then the running deployment is removed.
    let (r, _) = s.run(&["down"]).await;
    assert!(r.unwrap_err().downcast_ref::<UsageError>().is_some());
    let removed = s.json(&["down", "--yes"]).await;
    assert_eq!(removed["status"], "removed");

    // Things a token can't do say so clearly.
    let (r, _) = s.run(&["volume", "delete", "--yes"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("dashboard"), "{e}");
}
