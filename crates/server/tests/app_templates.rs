//! App templates: the gallery and creating an app's services in one go.

mod common;

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use sqlx::PgPool;

struct Ctx {
    app: TestApp,
    admin: String,
    ws: Id,
    project: String,
    prod: Id,
    staging: Id,
}

impl Ctx {
    async fn new(app: TestApp) -> Self {
        let admin = app.setup_admin().await;
        let ws = app.create_workspace(&admin, "team").await;
        let r = app
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(&admin),
                Some(json!({ "name": "apps" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let project = r.json["id"].as_str().unwrap().to_owned();
        let prod: Id = sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1")
            .bind(&project)
            .fetch_one(app.db())
            .await
            .unwrap();
        let r = app
            .call(
                Method::POST,
                &format!("/api/projects/{project}/environments"),
                Some(&admin),
                Some(json!({ "name": "staging" })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let staging: Id = r.json["id"].as_str().unwrap().parse().unwrap();
        Self {
            app,
            admin,
            ws,
            project,
            prod,
            staging,
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        self.app.call(method, path, Some(&self.admin), body).await
    }

    async fn deploy_app(&self, body: Value) -> Resp {
        self.call(
            Method::POST,
            &format!("/api/projects/{}/app-templates", self.project),
            Some(body),
        )
        .await
    }

    async fn resolved(&self, instance: Id, env: Id) -> BTreeMap<String, String> {
        modula_server::deploy::env::resolve(
            &self.app.state.keys,
            self.app.db(),
            None,
            env,
            instance,
        )
        .await
        .unwrap()
        .vars
        .into_iter()
        .map(|(k, v)| (k, v.expose_secret().to_owned()))
        .collect()
    }
}

fn id(v: &Value) -> Id {
    v.as_str().unwrap().parse().unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_gallery_lists_apps_and_their_services(pool: PgPool) {
    let c = Ctx::new(TestApp::new(pool)).await;
    let r = c.call(Method::GET, "/api/app-templates", None).await;
    assert_eq!(r.status, StatusCode::OK);
    let apps = r.json.as_array().unwrap();
    let ids: Vec<&str> = apps.iter().map(|a| a["id"].as_str().unwrap()).collect();
    for want in [
        "n8n",
        "plausible",
        "umami",
        "metabase",
        "uptime-kuma",
        "ghost",
    ] {
        assert!(ids.contains(&want), "{want}");
    }
    let plausible = apps.iter().find(|a| a["id"] == "plausible").unwrap();
    let roles: Vec<&str> = plausible["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles, ["postgres", "clickhouse", "plausible"]);
    assert_eq!(plausible["services"][0]["template"], "postgres");
    assert_eq!(plausible["services"][2]["domain"], true);
    // Nothing secret (variables, commands) is part of the listing.
    assert!(plausible["services"][2].get("variables").is_none());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn deploying_an_app_creates_its_services_wired_together(pool: PgPool) {
    let c = Ctx::new(TestApp::new(pool)).await;
    // A taken name: the app's database becomes postgres-2.
    let r = c
        .call(
            Method::POST,
            &format!("/api/projects/{}/services", c.project),
            Some(json!({ "name": "postgres", "template": "postgres" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let r = c
        .deploy_app(json!({ "template": "n8n", "environmentId": c.prod }))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let created = r.json;
    let names: Vec<&str> = created["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["postgres-2", "n8n"]);
    assert_eq!(created["services"][1]["web"], true);
    let domain = created["domain"].as_str().unwrap();
    assert!(
        domain.starts_with("n8n-") && domain.ends_with(".apps.test"),
        "{domain}"
    );
    assert_eq!(created["warnings"], json!([]));

    let db = id(&created["services"][0]["instanceId"]);
    let web = id(&created["services"][1]["instanceId"]);
    // Both services carry the app; the database keeps its template.
    let rows: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT name, template, app_template FROM services WHERE project_id = $1 ORDER BY name",
    )
    .bind(&c.project)
    .fetch_all(c.app.db())
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("n8n".into(), None, Some("n8n".into())),
            ("postgres".into(), Some("postgres".into()), None),
            (
                "postgres-2".into(),
                Some("postgres".into()),
                Some("n8n".into())
            ),
        ]
    );

    let r = c
        .call(Method::GET, &format!("/api/service-instances/{web}"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["volume"]["mountPath"], "/home/node/.n8n");
    assert_eq!(r.json["config"]["port"], 5678);
    assert_eq!(r.json["config"]["healthcheckPath"], "/healthz");
    assert!(
        r.json["config"]["source"]["image"]
            .as_str()
            .unwrap()
            .contains("@sha256:")
    );

    // The app reaches its own database, not the project's other one.
    let db_env = c.resolved(db, c.prod).await;
    let env = c.resolved(web, c.prod).await;
    assert_eq!(env["DB_POSTGRESDB_HOST"], "postgres-2.modula.internal");
    assert_eq!(env["DB_POSTGRESDB_PASSWORD"], db_env["POSTGRES_PASSWORD"]);
    assert_eq!(env["N8N_HOST"], domain);
    assert_eq!(env["WEBHOOK_URL"], format!("https://{domain}/"));
    assert_eq!(env["N8N_ENCRYPTION_KEY"].len(), 32);

    // Every environment gets the services, with its own secrets.
    let staging_web: Id = sqlx::query_scalar(
        "SELECT i.id FROM service_instances i JOIN services s ON s.id = i.service_id
         WHERE s.project_id = $1 AND s.name = 'n8n' AND i.environment_id = $2",
    )
    .bind(&c.project)
    .bind(c.staging)
    .fetch_one(c.app.db())
    .await
    .unwrap();
    let staging = c.resolved(staging_web, c.staging).await;
    assert_ne!(staging["N8N_ENCRYPTION_KEY"], env["N8N_ENCRYPTION_KEY"]);
    assert_ne!(
        staging["DB_POSTGRESDB_PASSWORD"],
        env["DB_POSTGRESDB_PASSWORD"]
    );
    // The domain is only in the environment it was deployed to.
    let domains: i64 =
        sqlx::query_scalar("SELECT count(*) FROM domains WHERE service_instance_id = $1")
            .bind(staging_web)
            .fetch_one(c.app.db())
            .await
            .unwrap();
    assert_eq!(domains, 0);

    // The project detail exposes the app for grouping on the canvas.
    let r = c
        .call(Method::GET, &format!("/api/projects/{}", c.project), None)
        .await;
    let svc = r.json["services"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "n8n")
        .unwrap();
    assert_eq!(svc["appTemplate"], "n8n");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_three_service_app_with_a_prefix(pool: PgPool) {
    let c = Ctx::new(TestApp::new(pool)).await;
    let r = c
        .deploy_app(
            json!({ "template": "plausible", "environmentId": c.staging, "namePrefix": "stats" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let names: Vec<&str> = r.json["services"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        ["stats-postgres", "stats-clickhouse", "stats-plausible"]
    );
    // Not the default environment: the domain names it.
    let domain = r.json["domain"].as_str().unwrap();
    assert!(domain.starts_with("stats-plausible-staging-"), "{domain}");
    let ch = id(&r.json["services"][1]["instanceId"]);
    let web = id(&r.json["services"][2]["instanceId"]);
    let ch_env = c.resolved(ch, c.staging).await;
    let env = c.resolved(web, c.staging).await;
    assert_eq!(
        env["CLICKHOUSE_DATABASE_URL"],
        format!(
            "http://plausible:{}@stats-clickhouse.modula.internal:8123/plausible_events_db",
            ch_env["CLICKHOUSE_PASSWORD"]
        )
    );
    assert!(
        env["DATABASE_URL"].starts_with("postgres://postgres:")
            && env["DATABASE_URL"].ends_with("@stats-postgres.modula.internal:5432/app"),
        "{}",
        env["DATABASE_URL"]
    );
    assert_eq!(env["SECRET_KEY_BASE"].len(), 64);
    assert_eq!(env["BASE_URL"], format!("https://{domain}"));

    // Again: every name gets a suffix.
    let r = c
        .deploy_app(
            json!({ "template": "plausible", "environmentId": c.staging, "namePrefix": "stats" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["services"][2]["name"], "stats-plausible-2");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn bad_requests_create_nothing(pool: PgPool) {
    let c = Ctx::new(TestApp::new(pool)).await;
    let count = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM services")
            .fetch_one(c.app.db())
            .await
            .unwrap()
    };
    let r = c
        .deploy_app(json!({ "template": "wordpress", "environmentId": c.prod }))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = c
        .deploy_app(
            json!({ "template": "ghost", "environmentId": c.prod, "namePrefix": "Bad_Prefix" }),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    // Another project's environment is not this project's.
    let r = c
        .call(
            Method::POST,
            &format!("/api/workspaces/{}/projects", c.ws),
            Some(json!({ "name": "other" })),
        )
        .await;
    let other: Id = sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1")
        .bind(r.json["id"].as_str().unwrap())
        .fetch_one(c.app.db())
        .await
        .unwrap();
    let r = c
        .deploy_app(json!({ "template": "ghost", "environmentId": other }))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND, "{:?}", r.json);
    assert_eq!(count().await, 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn without_an_apps_domain_the_app_is_created_with_a_warning(pool: PgPool) {
    let mut cfg = config();
    cfg.apps_domain = None;
    let c = Ctx::new(TestApp::with_config(pool, cfg)).await;
    let r = c
        .deploy_app(json!({ "template": "uptime-kuma", "environmentId": c.prod }))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["domain"], Value::Null);
    assert_eq!(r.json["warnings"].as_array().unwrap().len(), 1);
    let web = id(&r.json["services"][0]["instanceId"]);
    let r = c
        .call(Method::GET, &format!("/api/service-instances/{web}"), None)
        .await;
    assert_eq!(r.json["volume"]["mountPath"], "/app/data");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn viewers_and_deployers_cannot_deploy_apps(pool: PgPool) {
    let c = Ctx::new(TestApp::new(pool)).await;
    for role in ["viewer", "deployer"] {
        let (user, cookie) = c.app.user(&format!("{role}@example.com")).await;
        c.app.add_member(c.ws, user, role).await;
        let r = c
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{}/app-templates", c.project),
                Some(&cookie),
                Some(json!({ "template": "umami", "environmentId": c.prod })),
            )
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{role}: {:?}", r.json);
    }
}
