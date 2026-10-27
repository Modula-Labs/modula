//! `modula config` and `modula init` end to end: apply a
//! .modula/modula.toml against the real HTTP server with an API token, check
//! that the plan is then empty, that edits show the expected diff, and that
//! `config pull` round-trips (as TOML, and as TypeScript when Node or Bun is
//! available).

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;

use axum::http::{Method, StatusCode};
use clap::Parser;
use common::*;
use modula_cli::cli::Cli;
use modula_cli::commands;
use modula_cli::context::Session;
use modula_cli::iac::model::{CONFIG_KEYS, ConfigFile};
use modula_cli::iac::script::Runtime;
use modula_cli::output::Output;
use modula_id::Id;
use modula_server::deploy::config::ServiceConfig;
use serde_json::{Value, json};
use sqlx::PgPool;

struct Setup {
    app: TestApp,
    admin: String,
    api_url: String,
    token: String,
    dir: PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn setup(pool: PgPool) -> Setup {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
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
    let r = app
        .call(
            Method::POST,
            "/api/account/tokens",
            Some(&admin),
            Some(json!({ "name": "iac" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let dir = std::env::temp_dir().join(format!("modula-iac-{}", Id::new()));
    // A git repository, so the CLI keeps .modula/ out of it.
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    Setup {
        token: r.json["token"].as_str().unwrap().to_owned(),
        app,
        admin,
        api_url,
        dir,
    }
}

impl Setup {
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

    async fn ok(&self, args: &[&str]) -> String {
        match self.run(args).await {
            (Ok(0), text) => text,
            (Ok(code), text) => panic!("{args:?} exited {code}: {text}"),
            (Err(e), text) => panic!("{args:?} failed: {e:#}\n{text}"),
        }
    }

    async fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let text = self.ok(&all).await;
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{args:?}: {e}: {text}"))
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.dir.join(name), text).unwrap();
    }

    async fn get(&self, path: &str) -> Value {
        let r = self
            .app
            .call(Method::GET, path, Some(&self.admin), None)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {:?}", r.json);
        r.json
    }
}

const FILE: &str = r#"
project = "shop"

[services.api]
source = { image = "nginx:1.27" }
port = 80
healthcheckPath = "/health"
drainingSeconds = 30
domains = ["api.shop-example.com"]
generateDomain = true
volume = "/data"

[services.api.variables]
NODE_ENV = "production"
DB = "${{postgres.DATABASE_URL}}?schema=public"
AUTH_SECRET = { secret = true }
VAULT = { secret = true, sealed = true }

[services.postgres]
template = "postgres"
memoryMb = 512

[shared.variables]
REGION = "eu"

[environments.staging]
services.api.domains = ["staging.shop-example.com"]
services.api.variables.NODE_ENV = "staging"
"#;

fn env_plan<'a>(plan: &'a Value, env: &str) -> &'a Value {
    plan["environments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["environment"] == env)
        .unwrap_or_else(|| panic!("no {env} in {plan}"))
}

fn service<'a>(env: &'a Value, name: &str) -> &'a Value {
    env["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["service"] == name)
        .unwrap_or_else(|| panic!("no {name} in {env}"))
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn apply_plan_edit_and_pull(pool: PgPool) {
    let s = setup(pool).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.ok(&["init", "shop", "--workspace", "team"]).await;
    let gitignore = std::fs::read_to_string(s.dir.join(".modula/.gitignore")).unwrap();
    assert!(
        gitignore.ends_with("\nproject.toml\ntoken\nsecrets.*\n"),
        "{gitignore}"
    );
    // Without a terminal, init writes a config only when asked (--format).
    assert!(!s.dir.join(".modula/modula.ts").exists());

    s.write(".modula/modula.toml", FILE);
    s.ok(&["config", "validate"]).await;

    // Secrets come from gitignored files, one per environment.
    std::fs::write(
        s.dir.join(".modula/secrets.production.env"),
        "AUTH_SECRET=prod-secret-value\nVAULT=vault-value\n",
    )
    .unwrap();
    // A missing secret stops apply before anything changes.
    let (r, _) = s.run(&["config", "apply", "--all-envs", "--yes"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(
        e.contains("AUTH_SECRET") && e.contains("nothing was applied"),
        "{e}"
    );
    let projects = s.get(&format!("/api/workspaces/{ws}/projects")).await;
    let project = projects[0]["id"].as_str().unwrap().to_owned();
    let detail = s.get(&format!("/api/projects/{project}")).await;
    assert_eq!(detail["services"].as_array().unwrap().len(), 0);
    std::fs::write(
        s.dir.join(".modula/secrets.staging.env"),
        "AUTH_SECRET=staging-secret-value\nVAULT=vault-value\n",
    )
    .unwrap();

    // The plan: a new environment, two services, variables, domains.
    let human = s.ok(&["config", "plan", "--all-envs"]).await;
    assert!(
        human.contains("+ variable AUTH_SECRET (secret)\n"),
        "{human}"
    );
    assert!(
        human.contains("Environment shop/staging (new: will be created)"),
        "{human}"
    );
    let plan = s.json(&["config", "plan", "--all-envs"]).await;
    assert_eq!(plan["changes"], true);
    let staging = env_plan(&plan, "staging");
    assert_eq!(staging["createEnvironment"], true);
    let prod = env_plan(&plan, "production");
    assert_eq!(service(prod, "api")["action"], "create");
    let text = plan.to_string();
    assert!(!text.contains("prod-secret-value") && !text.contains("vault-value"));

    // Apply it.
    let applied = s.json(&["config", "apply", "--all-envs", "--yes"]).await;
    assert_eq!(applied["changes"], true);
    let detail = s.get(&format!("/api/projects/{project}")).await;
    let envs: Vec<&str> = detail["environments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(envs, ["production", "staging"]);
    let config = |env: &str| {
        let env_id = detail["environments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == env)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let s = &s;
        async move { s.get(&format!("/api/environments/{env_id}/config")).await }
    };
    let prod = config("production").await;
    let api = prod["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["serviceName"] == "api")
        .unwrap()
        .clone();
    assert_eq!(api["config"]["port"], 80);
    assert_eq!(api["config"]["drainingSeconds"], 30);
    assert_eq!(api["volume"]["mountPath"], "/data");
    let hosts: Vec<(&str, &str)> = api["domains"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["kind"].as_str().unwrap(), d["hostname"].as_str().unwrap()))
        .collect();
    assert!(
        hosts.contains(&("custom", "api.shop-example.com")),
        "{hosts:?}"
    );
    assert!(hosts.iter().any(|(k, _)| *k == "generated"), "{hosts:?}");
    let vars: Vec<(&str, bool, Option<&str>)> = api["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap(),
                v["sealed"].as_bool().unwrap(),
                v["value"].as_str(),
            )
        })
        .collect();
    assert!(
        vars.contains(&("AUTH_SECRET", false, Some("prod-secret-value"))),
        "{vars:?}"
    );
    assert!(
        vars.contains(&("VAULT", true, None)),
        "sealed values never come back"
    );
    assert!(vars.contains(&(
        "DB",
        false,
        Some("${{postgres.DATABASE_URL}}?schema=public")
    )));
    let pg = prod["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["serviceName"] == "postgres")
        .unwrap();
    assert_eq!(pg["template"], "postgres");
    assert_eq!(pg["config"]["memoryMb"], 512);
    assert_eq!(prod["shared"][0]["name"], "REGION");
    let staging = config("staging").await;
    let api_staging = staging["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["serviceName"] == "api")
        .unwrap();
    assert_eq!(
        api_staging["domains"][0]["hostname"],
        "staging.shop-example.com"
    );
    // Reading values is audited.
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'environment.config_read'",
    )
    .fetch_one(s.app.db())
    .await
    .unwrap();
    assert!(audited > 0);

    // Nothing left to do.
    let plan = s.json(&["config", "plan", "--all-envs"]).await;
    assert_eq!(plan["changes"], false, "{plan:#}");
    let text = s.ok(&["config", "plan"]).await;
    assert!(text.contains("no changes"), "{text}");

    // pull round-trips: the pulled file plans no changes either, and holds no secrets.
    let pulled = s.json(&["config", "pull", "--file", "pulled.toml"]).await;
    assert_eq!(pulled["guessedSecrets"], json!(["api.AUTH_SECRET"]));
    let text = std::fs::read_to_string(s.dir.join("pulled.toml")).unwrap();
    for secret in [
        "prod-secret-value",
        "staging-secret-value",
        "POSTGRES_PASSWORD",
    ] {
        assert!(!text.contains(secret), "{secret} in:\n{text}");
    }
    ConfigFile::load(&s.dir.join("pulled.toml")).unwrap();
    let plan = s
        .json(&["config", "plan", "--all-envs", "--file", "pulled.toml"])
        .await;
    assert_eq!(plan["changes"], false, "{plan:#}\n{text}");
    let (r, _) = s.run(&["config", "init", "--file", "pulled.toml"]).await;
    assert!(r.is_err(), "init doesn't overwrite without --force");

    // Edits show up as a diff.
    let edited = FILE
        .replace("port = 80\nhealthcheckPath = \"/health\"\n", "port = 8081\n")
        .replace(
            "DB = \"${{postgres.DATABASE_URL}}?schema=public\"",
            "NEW = 1",
        )
        .replace("domains = [\"api.shop-example.com\"]", "domains = [{ host = \"api.shop-example.com\", port = 9000 }, \"www.shop-example.com\"]");
    s.write(".modula/modula.toml", &edited);
    let human = s.ok(&["config", "plan", "-e", "production"]).await;
    assert!(
        human.contains("~ port: 80 → 8081") && human.contains("- variable DB"),
        "{human}"
    );
    let plan = s.json(&["config", "plan", "-e", "production"]).await;
    let prod = env_plan(&plan, "production");
    let api = service(prod, "api");
    assert_eq!(api["action"], "update");
    assert_eq!(
        api["settings"],
        json!([
            { "key": "healthcheckPath", "from": "/health", "to": null },
            { "key": "port", "from": 80, "to": 8081 },
        ])
    );
    let ops: Vec<(String, String)> = api["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap().into(),
                v["op"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        ops,
        [
            ("NEW".to_owned(), "add".to_owned()),
            ("DB".into(), "delete".into())
        ]
    );
    assert_eq!(
        api["domains"],
        json!([
            { "op": "update", "host": "api.shop-example.com", "from": null, "to": 9000 },
            { "op": "add", "host": "www.shop-example.com" },
        ])
    );
    assert_eq!(plan["environments"].as_array().unwrap().len(), 1);

    // Apply the edit; then it is clean again.
    s.ok(&["config", "apply", "-e", "production", "--yes"])
        .await;
    let plan = s.json(&["config", "plan", "-e", "production"]).await;
    assert_eq!(plan["changes"], false, "{plan:#}");

    // TypeScript: `config init` writes .modula/modula.ts (--force replaces
    // the TOML file), and it plans no changes in any environment.
    if let Err(e) = Runtime::detect() {
        eprintln!("skipping the modula.ts part: {e:#}");
        return;
    }
    let (r, _) = s.run(&["config", "init"]).await;
    assert!(r.is_err(), "one config file per .modula/");
    let written = s.json(&["config", "init", "--force"]).await;
    assert!(
        written["file"]
            .as_str()
            .unwrap()
            .ends_with(".modula/modula.ts"),
        "{written}"
    );
    assert!(!s.dir.join(".modula/modula.toml").exists());
    let ts = std::fs::read_to_string(s.dir.join(".modula/modula.ts")).unwrap();
    assert!(ts.contains("from \"@modula-labs/iac\""), "{ts}");
    let plan = s.json(&["config", "plan", "--all-envs"]).await;
    assert_eq!(plan["changes"], false, "{plan:#}\n{ts}");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn init_creates_or_links_and_writes_the_config(pool: PgPool) {
    let s = setup(pool).await;
    s.app.create_workspace(&s.admin, "team").await;

    // Without a terminal a name (or -p) is required; the error lists projects.
    let (r, _) = s.run(&["init", "--workspace", "team"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("pass a name") && e.contains("none yet"), "{e}");

    let created = s.json(&["init", "shop", "--workspace", "team"]).await;
    assert_eq!(created["name"], "shop");
    assert_eq!(created["created"], true);
    assert_eq!(created["environment"], "production");
    assert!(created["config"].is_null());
    s.ok(&["add", "--database", "postgres"]).await;

    // Linked already: needs --yes to set up again.
    let (r, _) = s.run(&["init", "-p", "shop", "--format", "toml"]).await;
    let e = format!("{:#}", r.unwrap_err());
    assert!(e.contains("already linked to shop"), "{e}");

    let linked = s
        .json(&["init", "-p", "shop", "--format", "toml", "--yes"])
        .await;
    assert_eq!(linked["created"], false);
    assert_eq!(linked["id"], created["id"]);
    let file = linked["config"]["file"].as_str().unwrap();
    assert!(file.ends_with(".modula/modula.toml"), "{linked}");
    let text = std::fs::read_to_string(file).unwrap();
    assert!(
        text.contains("[services.postgres]\ntemplate = \"postgres\""),
        "{text}"
    );
    let found = ConfigFile::find(&s.dir).unwrap().unwrap();
    assert_eq!(found, s.dir.join(".modula/modula.toml"));
    let plan = s.json(&["config", "plan"]).await;
    assert_eq!(plan["changes"], false, "{plan:#}\n{text}");

    // A config already there is kept.
    let again = s
        .json(&["init", "-p", "shop", "--format", "ts", "--yes"])
        .await;
    assert!(again["config"].is_null(), "{again}");
    assert!(!s.dir.join(".modula/modula.ts").exists());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn reading_an_environments_config_needs_member(pool: PgPool) {
    let s = setup(pool).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.ok(&["init", "shop", "--workspace", "team"]).await;
    let (viewer, cookie) = s.app.user("viewer@example.com").await;
    s.app.add_member(ws, viewer, "viewer").await;
    let projects = s.get(&format!("/api/workspaces/{ws}/projects")).await;
    let detail = s
        .get(&format!(
            "/api/projects/{}",
            projects[0]["id"].as_str().unwrap()
        ))
        .await;
    let env = detail["environments"][0]["id"].as_str().unwrap();
    let r = s
        .app
        .call(
            Method::GET,
            &format!("/api/environments/{env}/config"),
            Some(&cookie),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
}

/// The CLI's list of settings must match the server's `ServiceConfig`.
#[test]
fn the_cli_knows_every_service_setting() {
    // serde names every field in its unknown-field error.
    let err = serde_json::from_value::<ServiceConfig>(json!({
        "source": { "type": "image", "image": "nginx" },
        "notASetting": 1,
    }))
    .unwrap_err()
    .to_string();
    let listed = err.split("expected one of").nth(1).expect(&err);
    let mut server: Vec<&str> = listed
        .split(',')
        .map(|f| f.trim().trim_matches(|c| c == '`' || c == ' '))
        .filter(|f| !f.is_empty())
        .map(|f| f.split('`').next().unwrap())
        .collect();
    // `startCommandDetected` is kept by the server, never set from config.
    server.retain(|f| *f != "source" && *f != "startCommandDetected");
    server.sort_unstable();
    let mut cli: Vec<&str> = CONFIG_KEYS.to_vec();
    cli.sort_unstable();
    assert_eq!(
        cli, server,
        "update CONFIG_KEYS in crates/cli/src/iac/model.rs"
    );
}
