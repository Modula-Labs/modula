//! The `modula` CLI end to end: its command functions (parsed from
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

/// HTTP + node servers, one node (fake runtime, fake database, a fake
/// builder that really downloads and unpacks sources), a token.
async fn setup(pool: PgPool) -> Setup {
    // Nodes download uploaded sources from the real HTTP listener.
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}/", http.local_addr().unwrap());
    let mut cfg = config();
    cfg.internal_url = Some(api_url.parse().unwrap());
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    let stop = CancellationToken::new();

    let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_port = grpc.local_addr().unwrap().port();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        grpc,
        stop.clone(),
    ));
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
        build: Some(modula_node::build::BuildSupport {
            builder: Arc::new(modula_node::build::FakeBuilder {
                fetch: true,
                ..Default::default()
            }),
            logs: modula_node::build::BuildLogs::new(node_dir.join("build-logs")).unwrap(),
        }),
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
    /// Run `modula <args>` in the project directory; returns the exit
    /// code (or error) and what it printed.
    async fn run(&self, args: &[&str]) -> (anyhow::Result<i32>, String) {
        let cli = Cli::try_parse_from(std::iter::once("modula").chain(args.iter().copied()))
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
    assert!(link.contains("environment_name = \"production\""), "{link}");
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
    // Confirmed, it still needs Admin + sudo, which API tokens never have:
    // the CLI says to use the dashboard, and nothing reaches the database.
    let (r, _) = s
        .run(&["db", "query", "delete from t", "--write", "--yes"])
        .await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("do it in the dashboard"), "{e}");
    assert_eq!(s.db.calls().len(), calls, "nothing ran");

    s.db.answer(
        "tables",
        Ok(json!({ "tables": [{ "schema": "public", "name": "orders", "rowEstimate": 3 }] })),
    );
    let tables = s.ok(&["db", "tables"]).await;
    assert!(tables.contains("public  orders  3"), "{tables}");

    // Environments: duplicate production, then the link follows, by id:
    // renaming keeps it.
    s.ok(&["environment", "new", "staging", "--duplicate", "production"])
        .await;
    let status = s.json(&["status"]).await;
    assert_eq!(status["environment"]["name"], "staging");
    assert!(status["services"][0]["status"].is_null());
    s.ok(&["environment", "rename", "staging", "preview"]).await;
    let status = s.json(&["status"]).await;
    assert_eq!(status["environment"]["name"], "preview");
    // Deleting needs sudo, which API tokens never have.
    let (r, _) = s.run(&["environment", "delete", "preview", "--yes"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("/settings/environments?env="), "{e}");
    s.ok(&["environment", "production"]).await;
    let envs = s.json(&["environment"]).await;
    assert_eq!(envs.as_array().unwrap().len(), 2);
    assert_eq!(envs[0]["default"], true);

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

/// `up`: the directory is packed (ignore files honored), uploaded, built by
/// the node from a signed download, and deployed; then redeployed from the
/// kept upload.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn up_uploads_local_files_and_deploys_them(pool: PgPool) {
    let s = setup(pool).await;
    s.app.create_workspace(&s.admin, "team").await;
    s.json(&["init", "web", "--workspace", "team"]).await;
    // An image service: `up` builds the upload anyway, and leaves its source alone.
    s.json(&["add", "--image", "nginx:alpine", "--name", "site"])
        .await;

    // The app lives in a subdirectory (the project dir also holds the CLI's
    // config file, which must never be uploaded anyway).
    let app = s.dir.join("site");
    let write = |rel: &str, body: &[u8]| {
        let p = app.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    write("package.json", b"{}");
    write("src/server.js", b"require('http')");
    write(".gitignore", b"*.log\nnode_modules/\n");
    write("debug.log", b"noise");
    write("node_modules/dep/index.js", b"x");
    write(".modulaignore", b"secret.txt\n");
    write("secret.txt", b"hunter2");
    // Over 1 MB of incompressible bytes: the global body limit doesn't apply.
    let mut big = Vec::with_capacity(1_500_000);
    let mut x: u32 = 1;
    for _ in 0..1_500_000 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        big.push(x as u8);
    }
    write("assets/blob.bin", &big);

    let d = s
        .json(&["up", "--path", "site", "--ci", "-m", "First upload"])
        .await;
    assert_eq!(d["status"], "active", "{d}");
    assert_eq!(d["sourceKind"], "upload", "{d}");
    assert_eq!(d["source"], "Uploaded via CLI");
    assert_eq!(d["trigger"], "upload");
    assert_eq!(d["commitMessage"], "First upload");
    assert_eq!(d["uploadAvailable"], true);
    let id = d["id"].as_str().unwrap().to_owned();

    // The node built exactly the packed files.
    let logs = s.ok(&["logs", "--deployment", &id, "--build"]).await;
    for want in [
        "file package.json",
        "file src/server.js",
        "file .gitignore",
        "file assets/blob.bin",
    ] {
        assert!(logs.contains(want), "{want} missing:\n{logs}");
    }
    for gone in ["debug.log", "node_modules", "secret.txt", "config.toml"] {
        assert!(!logs.contains(gone), "{gone} was uploaded:\n{logs}");
    }

    // The service's own source is unchanged.
    let config: Value = sqlx::query_scalar(
        "SELECT i.config FROM service_instances i JOIN services s ON s.id = i.service_id WHERE s.name = 'site'",
    )
    .fetch_one(s.app.db())
    .await
    .unwrap();
    assert_eq!(config["source"]["type"], "image", "{config}");

    // A day later the upload is still kept: its deployment is live (and
    // among the service's newest), so a redeploy can rebuild it.
    sqlx::query("UPDATE source_uploads SET created_at = now() - interval '25 hours'")
        .execute(s.app.db())
        .await
        .unwrap();
    let purged = modula_server::deploy::upload::purge(&s.app.state)
        .await
        .unwrap();
    assert_eq!(purged, 0);
    let listed = s.json(&["deployment", "list"]).await;
    assert_eq!(listed[0]["uploadAvailable"], true, "{listed}");

    // The upload is kept: a redeploy rebuilds it.
    let again = s.json(&["redeploy", "--wait"]).await;
    assert_eq!(again["status"], "active", "{again}");
    assert_eq!(again["sourceKind"], "upload");

    // Once purged, it can't be rebuilt.
    sqlx::query("UPDATE source_uploads SET purged_at = now()")
        .execute(s.app.db())
        .await
        .unwrap();
    let (r, _) = s.run(&["redeploy"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("no longer kept"), "{e}");

    // The audit log has the upload.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'deployment.upload'")
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert_eq!(n, 1);
}

/// `add --template`: an app and its database, created and deployed together.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn add_template_creates_and_deploys_an_app(pool: PgPool) {
    let s = setup(pool).await;
    s.app.create_workspace(&s.admin, "team").await;
    s.json(&["init", "web", "--workspace", "team"]).await;

    let (r, _) = s.run(&["add", "--template", "wordpress"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("one of: ghost, metabase, n8n"), "{e}");
    let listed = s.json(&["add", "--template", "list"]).await;
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "umami")
    );

    let created = s
        .json(&["add", "--template", "umami", "--name", "stats"])
        .await;
    let names: Vec<&str> = created["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["stats-postgres", "stats-umami"]);
    assert!(created["domain"].as_str().unwrap().ends_with(".apps.test"));
    // Each service either deployed or says why not.
    let tried = created["deployments"].as_array().unwrap().len()
        + created["warnings"].as_array().unwrap().len();
    assert_eq!(tried, 2, "{created}");
}

/// Run `modula <args>` with nothing in the environment: the saved logins
/// in `dir/config.toml` decide.
async fn run_saved(dir: &std::path::Path, args: &[&str]) -> (anyhow::Result<i32>, String) {
    let cli = Cli::try_parse_from(std::iter::once("modula").chain(args.iter().copied()))
        .expect("valid arguments");
    let (out, buf) = Output::capture(cli.global.json);
    let session = Session {
        config_path: dir.join("config.toml"),
        cwd: dir.to_path_buf(),
        env_url: None,
        env_token: None,
        interactive: false,
    };
    let r = commands::run(cli, session, out).await;
    let text = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    (r, text)
}

async fn saved_json(dir: &std::path::Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend_from_slice(args);
    let (r, text) = run_saved(dir, &all).await;
    match r {
        Ok(0) => serde_json::from_str(&text).unwrap_or_else(|e| panic!("{args:?}: {e}: {text}")),
        other => panic!("{args:?}: {other:?}\n{text}"),
    }
}

/// `login` with a token and in the browser, named logins, `--login`, and the
/// login a linked directory remembers.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn named_logins_token_and_browser(pool: PgPool) {
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}/", http.local_addr().unwrap());
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(
            http,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let mut tokens = Vec::new();
    for name in ["a", "b"] {
        let r = app
            .call(
                Method::POST,
                "/api/account/tokens",
                Some(&admin),
                Some(json!({ "name": name })),
            )
            .await;
        tokens.push(r.json["token"].as_str().unwrap().to_owned());
    }
    let (ta, tb) = (&tokens[0], &tokens[1]);
    let dir = std::env::temp_dir().join(format!("modula-cli-logins-{}", Id::new()));
    std::fs::create_dir_all(&dir).unwrap();

    // Two logins with tokens (the CI path); the newest is the default.
    let a = saved_json(&dir, &["login", &api_url, "--token", ta, "--name", "a"]).await;
    assert_eq!(a["login"], "a");
    assert_eq!(a["user"]["email"], "admin@example.com");
    saved_json(&dir, &["login", &api_url, "--token", tb, "--name", "b"]).await;
    let cfg = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(cfg.starts_with("current = \"b\""), "{cfg}");
    assert!(
        cfg.contains("[logins.a]") && cfg.contains(ta.as_str()),
        "{cfg}"
    );
    assert_eq!(saved_json(&dir, &["whoami"]).await["login"], "b");
    assert_eq!(
        saved_json(&dir, &["--login", "a", "whoami"]).await["login"],
        "a"
    );
    let (r, _) = run_saved(&dir, &["--login", "nope", "whoami"]).await;
    assert!(format!("{:#}", r.unwrap_err()).contains("no login named nope"));
    let listed = saved_json(&dir, &["logins"]).await;
    assert_eq!(listed[0]["name"], "a");
    assert_eq!(listed[1]["current"], true);
    saved_json(&dir, &["login", "use", "a"]).await;
    assert_eq!(saved_json(&dir, &["whoami"]).await["login"], "a");
    // A malformed token is refused before anything is saved.
    let (r, _) = run_saved(&dir, &["login", &api_url, "--token", "nope", "--name", "x"]).await;
    assert!(r.unwrap_err().downcast_ref::<UsageError>().is_some());

    // The browser flow: approved in the dashboard while the CLI polls.
    let cli_dir = dir.clone();
    let url = api_url.clone();
    let login = tokio::spawn(async move {
        run_saved(&cli_dir, &["login", &url, "--browserless", "--name", "c"]).await
    });
    let mut code = None;
    for _ in 0..100 {
        code = sqlx::query_scalar::<_, String>(
            "SELECT user_code FROM cli_logins WHERE status = 'pending'",
        )
        .fetch_optional(app.db())
        .await
        .unwrap();
        if code.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let r = app
        .call(
            Method::POST,
            "/api/cli/login/approve",
            Some(&admin),
            Some(json!({ "userCode": code.expect("a pending login") })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let (r, text) = login.await.unwrap();
    assert_eq!(r.unwrap(), 0, "{text}");
    assert!(text.contains("saved as c"), "{text}");
    let listed = app
        .call(Method::GET, "/api/account/tokens", Some(&admin), None)
        .await;
    assert!(
        listed.json[0]["name"]
            .as_str()
            .unwrap()
            .starts_with("CLI on "),
        "{}",
        listed.json
    );
    assert_eq!(saved_json(&dir, &["whoami"]).await["login"], "c");

    // Linking records the login in use; the directory keeps using it.
    let ws = app.create_workspace(&admin, "team").await;
    let r = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(&admin),
            Some(json!({ "name": "shop" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    saved_json(&dir, &["--login", "a", "link", "-p", "shop"]).await;
    let link = std::fs::read_to_string(dir.join(".modula/project.toml")).unwrap();
    assert!(link.contains("login = \"a\""), "{link}");
    assert_eq!(saved_json(&dir, &["whoami"]).await["login"], "a");
    assert_eq!(
        saved_json(&dir, &["--login", "b", "whoami"]).await["login"],
        "b"
    );
    // Once that login is gone, the directory says so.
    let out = saved_json(&dir, &["logout", "a"]).await;
    assert_eq!(out["loggedOut"], "a");
    let (r, _) = run_saved(&dir, &["status"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("linked with the login a"), "{e}");

    saved_json(&dir, &["logout", "--all"]).await;
    assert_eq!(saved_json(&dir, &["logins"]).await, json!([]));
    let _ = std::fs::remove_dir_all(&dir);
}
