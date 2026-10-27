//! Projects, environments, services and variables over the HTTP API:
//! write-only secrets, per-environment encryption, and role checks.

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use serde_json::{Value, json};
use sqlx::PgPool;

struct Fixture {
    app: TestApp,
    admin: String,
    workspace: Id,
    project: String,
    instance: String,
}

async fn fixture(pool: PgPool) -> Fixture {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let workspace = app.create_workspace(&admin, "team").await;
    let p = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{workspace}/projects"),
            Some(&admin),
            Some(json!({"name": "shop"})),
        )
        .await;
    assert_eq!(p.status, StatusCode::OK, "{:?}", p.json);
    assert_eq!(p.json["environments"][0]["name"], "production");
    let project = p.json["id"].as_str().unwrap().to_owned();
    let s = app
        .call(
            Method::POST,
            &format!("/api/projects/{project}/services"),
            Some(&admin),
            Some(json!({ "name": "api", "config": { "source": { "type": "image", "image": "nginx:alpine" } } })),
        )
        .await;
    assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
    let instance = s.json["instances"][0]["id"].as_str().unwrap().to_owned();
    Fixture {
        app,
        admin,
        workspace,
        project,
        instance,
    }
}

impl Fixture {
    async fn set_vars(&self, cookie: &str, body: Value) -> Resp {
        self.app
            .call(
                Method::PATCH,
                &format!("/api/service-instances/{}/variables", self.instance),
                Some(cookie),
                Some(body),
            )
            .await
    }

    async fn var_id(&self, name: &str) -> String {
        let r = self
            .app
            .call(
                Method::GET,
                &format!("/api/service-instances/{}/variables", self.instance),
                Some(&self.admin),
                None,
            )
            .await;
        r.json
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == name)
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn variables_are_write_only_and_reveals_are_audited(pool: PgPool) {
    let f = fixture(pool).await;
    let r = f
        .set_vars(&f.admin, json!({ "set": { "DATABASE_URL": "postgres://u:hunter2@db/x", "STRIPE_KEY": "sk_test_fake_123456" }, "sealed": ["STRIPE_KEY"] }))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let listed = r.json.to_string();
    assert!(
        !listed.contains("hunter2") && !listed.contains("sk_test_fake"),
        "{listed}"
    );
    assert_eq!(r.json.as_array().unwrap().len(), 2);

    // Stored encrypted, under the environment's own data key.
    let raw: Vec<u8> =
        sqlx::query_scalar("SELECT value_sealed FROM variables WHERE name = 'DATABASE_URL'")
            .fetch_one(f.app.db())
            .await
            .unwrap();
    assert!(!String::from_utf8_lossy(&raw).contains("hunter2"));
    let env_key: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM data_keys k JOIN environments e ON e.id = k.scope WHERE e.project_id = $1",
    )
    .bind(&f.project)
    .fetch_one(f.app.db())
    .await
    .unwrap();
    assert_eq!(env_key, 1);

    // Reveal needs sudo, works for normal variables, never for sealed ones.
    let url = f.var_id("DATABASE_URL").await;
    let sealed = f.var_id("STRIPE_KEY").await;
    f.app.drop_sudo().await;
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/variables/{url}/reveal"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(r.code(), "sudo_required");
    let fresh = f.app.login("admin@example.com").await;
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/variables/{url}/reveal"),
            Some(&fresh),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["value"], "postgres://u:hunter2@db/x");
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/variables/{sealed}/reveal"),
            Some(&fresh),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // Sealed is permanent, even when the value is replaced.
    f.set_vars(
        &fresh,
        json!({ "set": { "STRIPE_KEY": "sk_test_fake_rotated" } }),
    )
    .await;
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/variables/{sealed}/reveal"),
            Some(&fresh),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    // The audit log records names, never values.
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, metadata FROM audit_log WHERE workspace_id = $1 ORDER BY id",
    )
    .bind(f.workspace)
    .fetch_all(f.app.db())
    .await
    .unwrap();
    assert!(
        audit
            .iter()
            .any(|(a, m)| a == "variable.reveal" && m["name"] == "DATABASE_URL")
    );
    let all = serde_json::to_string(&audit).unwrap();
    assert!(
        !all.contains("hunter2") && !all.contains("sk_test_fake"),
        "{all}"
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn variables_can_be_deleted(pool: PgPool) {
    let f = fixture(pool).await;
    f.set_vars(&f.admin, json!({ "set": { "A": "1", "B": "2", "C": "3" } }))
        .await;
    let r = f
        .set_vars(
            &f.admin,
            json!({ "delete": ["A", "C"], "set": { "B": "22" } }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let names: Vec<&str> = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["B"]);
    // Deleting a name that doesn't exist is a no-op, not an error.
    let r = f.set_vars(&f.admin, json!({ "delete": ["NOPE"] })).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn ciphertext_is_bound_to_its_row(pool: PgPool) {
    let f = fixture(pool).await;
    f.set_vars(
        &f.admin,
        json!({ "set": { "A": "value-of-a", "B": "value-of-b" } }),
    )
    .await;
    // Swap the ciphertexts: each must now fail to decrypt rather than reveal
    // the other variable's value under the wrong name.
    sqlx::query(
        "UPDATE variables v SET value_sealed = o.value_sealed
         FROM variables o WHERE o.service_instance_id = v.service_instance_id AND o.name <> v.name",
    )
    .execute(f.app.db())
    .await
    .unwrap();
    let a = f.var_id("A").await;
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/variables/{a}/reveal"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(!r.json.to_string().contains("value-of"));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn roles_gate_variables_and_deploys(pool: PgPool) {
    let f = fixture(pool).await;
    f.set_vars(&f.admin, json!({ "set": { "TOKEN": "secret-token-1" } }))
        .await;
    let token = f.var_id("TOKEN").await;
    let (viewer_id, viewer) = f.app.user("viewer@example.com").await;
    let (deployer_id, deployer) = f.app.user("deployer@example.com").await;
    f.app.add_member(f.workspace, viewer_id, "viewer").await;
    f.app.add_member(f.workspace, deployer_id, "deployer").await;
    let vars = format!("/api/service-instances/{}/variables", f.instance);
    let deploys = format!("/api/service-instances/{}/deployments", f.instance);
    let reveal = format!("/api/variables/{token}/reveal");

    // Viewer: names only.
    assert_eq!(
        f.app
            .call(Method::GET, &vars, Some(&viewer), None)
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        f.set_vars(&viewer, json!({ "set": { "X": "1" } }))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        f.app
            .call(Method::POST, &deploys, Some(&viewer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );

    // Deployer: may set variables and deploy, but not read values or change settings.
    assert_eq!(
        f.set_vars(&deployer, json!({ "set": { "X": "1" } }))
            .await
            .status,
        StatusCode::OK
    );
    let r = f
        .app
        .call(Method::POST, &deploys, Some(&deployer), None)
        .await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "authorized, but there is no server yet: {:?}",
        r.json
    );
    assert_eq!(
        f.app
            .call(Method::POST, &reveal, Some(&deployer), None)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let r = f
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{}", f.instance),
            Some(&deployer),
            Some(json!({ "config": { "source": { "type": "image", "image": "evil/image" } } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn environments_copy_config_and_keep_their_own_keys(pool: PgPool) {
    let f = fixture(pool).await;
    f.set_vars(
        &f.admin,
        json!({ "set": { "SECRET": "only-in-production" } }),
    )
    .await;
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/projects/{}/environments", f.project),
            Some(&f.admin),
            Some(json!({"name": "Staging"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["name"], "staging");
    let staging = r.json["id"].as_str().unwrap().to_owned();
    let dup = f
        .app
        .call(
            Method::POST,
            &format!("/api/projects/{}/environments", f.project),
            Some(&f.admin),
            Some(json!({"name": "staging"})),
        )
        .await;
    assert_eq!(dup.status, StatusCode::CONFLICT);

    // The service exists in staging with the same config but no variables.
    let p = f
        .app
        .call(
            Method::GET,
            &format!("/api/projects/{}", f.project),
            Some(&f.admin),
            None,
        )
        .await;
    let staged = p.json["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["environmentId"] == staging.as_str())
        .unwrap();
    let si = staged["id"].as_str().unwrap();
    let inst = f
        .app
        .call(
            Method::GET,
            &format!("/api/service-instances/{si}"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(inst.json["config"]["source"]["image"], "nginx:alpine");
    let vars = f
        .app
        .call(
            Method::GET,
            &format!("/api/service-instances/{si}/variables"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(vars.json.as_array().unwrap().len(), 0);

    // Deleting an environment destroys its data key; the default one can't go.
    f.app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{si}/variables"),
            Some(&f.admin),
            Some(json!({"set": {"S": "staging-only"}})),
        )
        .await;
    let r = f
        .app
        .call(
            Method::DELETE,
            &format!("/api/environments/{staging}"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM data_keys WHERE scope = $1")
        .bind(&staging)
        .fetch_one(f.app.db())
        .await
        .unwrap();
    assert_eq!(keys, 0);
    let prod = p.json["environments"][0]["id"].as_str().unwrap();
    let r = f
        .app
        .call(
            Method::DELETE,
            &format!("/api/environments/{prod}"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn bad_input_is_rejected(pool: PgPool) {
    let f = fixture(pool).await;
    let services = format!("/api/projects/{}/services", f.project);
    for (name, image) in [
        ("API", "nginx"),
        ("-x", "nginx"),
        ("ok", "nginx; rm -rf /"),
        ("ok2", "../etc"),
    ] {
        let r = f
            .app
            .call(
                Method::POST,
                &services,
                Some(&f.admin),
                Some(json!({ "name": name, "config": { "source": { "type": "image", "image": image } } })),
            )
            .await;
        assert_eq!(
            r.status,
            StatusCode::BAD_REQUEST,
            "{name} {image}: {:?}",
            r.json
        );
    }
    // Privileged container options don't exist in the API at all.
    let r = f
        .app
        .call(
            Method::POST,
            &services,
            Some(&f.admin),
            Some(json!({ "name": "x", "config": { "source": { "type": "image", "image": "nginx" }, "privileged": true } })),
        )
        .await;
    assert!(r.status.is_client_error());
    for body in [
        json!({ "set": { "MODULA_TOKEN": "x" } }),
        json!({ "set": { "1BAD": "x" } }),
        json!({ "set": { "A": "x" }, "delete": ["A"] }),
        json!({ "sealed": ["NOT_SET"] }),
        json!({ "set": { "BIG": "x".repeat(40 * 1024) } }),
    ] {
        let r = f.set_vars(&f.admin, body.clone()).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}");
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn activity_records_names_and_sync_copies_config(pool: PgPool) {
    let f = fixture(pool).await;
    f.set_vars(&f.admin, json!({ "set": { "A": "1", "B": "2" } }))
        .await;
    f.set_vars(&f.admin, json!({ "set": { "A": "3" }, "delete": ["B"] }))
        .await;
    let r = f
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{}", f.instance),
            Some(&f.admin),
            Some(json!({ "config": { "source": { "type": "image", "image": "nginx:alpine" }, "port": 3000 } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let feed = |query: &'static str| {
        let app = &f.app;
        let (admin, project) = (f.admin.clone(), f.project.clone());
        async move {
            let r = app
                .call(
                    Method::GET,
                    &format!("/api/projects/{project}/activity{query}"),
                    Some(&admin),
                    None,
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
            r.json["items"].as_array().unwrap().clone()
        }
    };
    let lines = |items: &[Value]| -> Vec<String> {
        items
            .iter()
            .flat_map(|g| g["changes"].as_array().unwrap().clone())
            .map(|c| {
                format!(
                    "{} {} {}",
                    c["kind"].as_str().unwrap(),
                    c["op"].as_str().unwrap(),
                    c["key"].as_str().unwrap_or("")
                )
            })
            .collect()
    };
    let all = feed("").await;
    assert_eq!(
        lines(&all),
        [
            "config updated port",
            "variable updated A",
            "variable removed B",
            "variable added A",
            "variable added B",
            "service created api",
        ]
    );
    assert_eq!(all[1]["serviceName"], "api");
    assert_eq!(all[1]["actor"]["name"], "Admin");
    assert!(
        !Value::from(all.clone()).to_string().contains("\"3\""),
        "values are never recorded"
    );
    assert_eq!(lines(&feed("?kind=config").await), ["config updated port"]);
    let bad = f
        .app
        .call(
            Method::GET,
            &format!("/api/projects/{}/activity?kind=bogus", f.project),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);

    // Sync: staging takes production's config and the variables it lacks.
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/projects/{}/environments", f.project),
            Some(&f.admin),
            Some(json!({"name": "staging"})),
        )
        .await;
    let staging = r.json["id"].as_str().unwrap().to_owned();
    let prod: String =
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE id = $1")
            .bind(&f.instance)
            .fetch_one(&f.app.state.db)
            .await
            .unwrap();
    f.set_vars(&f.admin, json!({ "set": { "C": "4" }, "sealed": ["C"] }))
        .await;
    let sync = |from: String| {
        let app = &f.app;
        let (admin, staging) = (f.admin.clone(), staging.clone());
        async move {
            app.call(
                Method::POST,
                &format!("/api/environments/{staging}/sync"),
                Some(&admin),
                Some(json!({ "fromEnvironmentId": from })),
            )
            .await
        }
    };
    let r = sync(prod.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(
        r.json,
        json!({ "created": 1, "updated": 0 }),
        "A is copied; sealed C is not"
    );
    let r = sync(prod).await;
    assert_eq!(r.json, json!({ "created": 0, "updated": 0 }));
    assert_eq!(sync(staging.clone()).await.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn duplicates_copy_every_variable_and_keep_sealed_ones_sealed(pool: PgPool) {
    use secrecy::ExposeSecret;
    let f = fixture(pool).await;
    f.set_vars(
        &f.admin,
        json!({ "set": { "PLAIN": "1", "SECRET": "two" }, "sealed": ["SECRET"] }),
    )
    .await;
    let prod = f.production().await;
    let shared = f
        .app
        .call(
            Method::PATCH,
            &format!("/api/environments/{prod}/variables"),
            Some(&f.admin),
            Some(json!({ "set": { "REGION": "eu" } })),
        )
        .await;
    assert_eq!(shared.status, StatusCode::OK, "{:?}", shared.json);
    let db = f.app.state.db.clone();
    let vars = |env: Id| {
        let db = db.clone();
        async move {
            sqlx::query_as::<_, (Id, String, bool, Vec<u8>)>(
                "SELECT id, name, sealed, value_sealed FROM variables WHERE environment_id = $1 ORDER BY name",
            )
            .bind(env)
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };

    let empty = f.create_env(json!({ "name": "empty" })).await;
    assert!(
        vars(empty).await.is_empty(),
        "an empty environment has no variables"
    );

    let staging = f
        .create_env(json!({ "name": "staging", "duplicateFrom": prod }))
        .await;
    let copied = vars(staging).await;
    let names: Vec<(&str, bool)> = copied.iter().map(|v| (v.1.as_str(), v.2)).collect();
    assert_eq!(
        names,
        [("PLAIN", false), ("REGION", false), ("SECRET", true)]
    );
    // Re-encrypted under the new environment's own key, same value.
    let (id, _, _, sealed) = copied.iter().find(|v| v.1 == "SECRET").unwrap();
    let keys = &f.app.state.keys;
    let value = modula_server::deploy::env::open_variable(keys, &db, staging, *id, sealed)
        .await
        .unwrap();
    assert_eq!(value.expose_secret(), "two");
    assert!(
        modula_server::deploy::env::open_variable(keys, &db, prod, *id, sealed)
            .await
            .is_err(),
        "the copy is sealed with the new environment's key"
    );
    // Still write-only over the API.
    let si: String =
        sqlx::query_scalar("SELECT id FROM service_instances WHERE environment_id = $1")
            .bind(staging)
            .fetch_one(&db)
            .await
            .unwrap();
    let r = f
        .app
        .call(
            Method::GET,
            &format!("/api/service-instances/{si}/variables"),
            Some(&f.admin),
            None,
        )
        .await;
    let secret = r
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "SECRET")
        .unwrap();
    assert_eq!(secret["sealed"], true, "{secret:?}");
    assert!(secret.get("value").is_none_or(Value::is_null), "{secret:?}");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn new_environments_can_deploy_another_branch(pool: PgPool) {
    let f = fixture(pool).await;
    let prod = f.production().await;
    let service: String =
        sqlx::query_scalar("SELECT service_id FROM service_instances WHERE id = $1")
            .bind(&f.instance)
            .fetch_one(f.app.db())
            .await
            .unwrap();
    let create = |body: Value| {
        let (app, admin, project) = (&f.app, f.admin.clone(), f.project.clone());
        async move {
            app.call(
                Method::POST,
                &format!("/api/projects/{project}/environments"),
                Some(&admin),
                Some(body),
            )
            .await
        }
    };
    // An image service has no branch.
    let r = create(json!({ "name": "x", "branches": { &service: "dev" } })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    sqlx::query("UPDATE service_instances SET config = $2 WHERE id = $1")
        .bind(&f.instance)
        .bind(json!({ "source": { "type": "repo", "repo": "acme/app", "branch": "main" } }))
        .execute(f.app.db())
        .await
        .unwrap();
    let r = create(json!({ "name": "x", "branches": { &service: "bad branch" } })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = create(json!({ "name": "x", "branches": { Id::new().to_string(): "dev" } })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let exists: i64 = sqlx::query_scalar("SELECT count(*) FROM environments WHERE name = 'x'")
        .fetch_one(f.app.db())
        .await
        .unwrap();
    assert_eq!(exists, 0, "a rejected request creates nothing");

    let branch = |env: Id| {
        let db = f.app.state.db.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT config->'source'->>'branch' FROM service_instances WHERE environment_id = $1",
            )
            .bind(env)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let dev = f
        .create_env(
            json!({ "name": "dev", "duplicateFrom": prod, "branches": { &service: "dev" } }),
        )
        .await;
    assert_eq!(branch(dev).await, "dev");
    assert_eq!(branch(prod).await, "main", "the source keeps its branch");
    let same = f
        .create_env(json!({ "name": "same", "duplicateFrom": prod }))
        .await;
    assert_eq!(branch(same).await, "main");

    // The project detail carries each instance's branch (for the dialog).
    let p = f
        .app
        .call(
            Method::GET,
            &format!("/api/projects/{}", f.project),
            Some(&f.admin),
            None,
        )
        .await;
    let dev_instance = p.json["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["environmentId"] == dev.to_string())
        .unwrap();
    assert_eq!(dev_instance["branch"], "dev");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn environments_rename_and_the_default_stays(pool: PgPool) {
    let f = fixture(pool).await;
    let prod = f.production().await;
    let staging = f.create_env(json!({ "name": "staging" })).await;
    let rename = |env: Id, name: &'static str| {
        let (app, admin) = (&f.app, f.admin.clone());
        async move {
            app.call(
                Method::PATCH,
                &format!("/api/environments/{env}"),
                Some(&admin),
                Some(json!({ "name": name })),
            )
            .await
        }
    };
    let r = rename(staging, "Preview").await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["name"], "preview");
    assert_eq!(r.json["isDefault"], false);
    assert_eq!(
        rename(staging, "production").await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(rename(staging, " ").await.status, StatusCode::BAD_REQUEST);

    // The default can be renamed but stays the default: not deletable.
    let r = rename(prod, "live").await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["isDefault"], true);
    let r = f
        .app
        .call(
            Method::DELETE,
            &format!("/api/environments/{prod}"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    let r = f
        .app
        .call(
            Method::DELETE,
            &format!("/api/environments/{staging}"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn environment_changes_need_their_roles(pool: PgPool) {
    let f = fixture(pool).await;
    let prod = f.production().await;
    let staging = f.create_env(json!({ "name": "staging" })).await;
    let mut users = Vec::new();
    for role in ["viewer", "deployer", "member"] {
        let (id, cookie) = f.app.user(&format!("{role}@example.com")).await;
        f.app.add_member(f.workspace, id, role).await;
        users.push((role, cookie));
    }
    let create = format!("/api/projects/{}/environments", f.project);
    let env = format!("/api/environments/{staging}");
    for (role, cookie) in &users {
        let r = f
            .app
            .call(
                Method::POST,
                &create,
                Some(cookie),
                Some(json!({ "name": format!("by-{role}"), "duplicateFrom": prod })),
            )
            .await;
        let rename = f
            .app
            .call(
                Method::PATCH,
                &env,
                Some(cookie),
                Some(json!({ "name": format!("{role}-env") })),
            )
            .await;
        let delete = f.app.call(Method::DELETE, &env, Some(cookie), None).await;
        let member = *role == "member";
        assert_eq!(
            r.status == StatusCode::OK,
            member,
            "{role} create: {:?}",
            r.json
        );
        assert_eq!(
            rename.status == StatusCode::OK,
            member,
            "{role} rename: {:?}",
            rename.json
        );
        assert_eq!(delete.status, StatusCode::FORBIDDEN, "{role} delete");
        assert_ne!(delete.code(), "sudo_required", "{role}");
    }
    // Admins delete with a recent re-authentication only.
    f.app.drop_sudo().await;
    let r = f.app.call(Method::DELETE, &env, Some(&f.admin), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);
    assert_eq!(r.code(), "sudo_required");
}

impl Fixture {
    /// Create an environment of the project (admin); returns its id.
    async fn create_env(&self, body: Value) -> Id {
        let r = self
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{}/environments", self.project),
                Some(&self.admin),
                Some(body),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json["id"].as_str().unwrap().parse().unwrap()
    }

    /// A one-click database service; returns its instance in production.
    async fn template_service(&self, template: &str) -> Value {
        let r = self
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{}/services", self.project),
                Some(&self.admin),
                Some(json!({ "name": template, "template": template })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json["instances"][0].clone()
    }

    async fn production(&self) -> Id {
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE id = $1")
            .bind(&self.instance)
            .fetch_one(&self.app.state.db)
            .await
            .unwrap()
    }

    async fn resolve(&self, env: Id, instance: &str) -> modula_server::deploy::env::Resolved {
        modula_server::deploy::env::resolve(
            &self.app.state.keys,
            &self.app.state.db,
            None,
            env,
            instance.parse().unwrap(),
        )
        .await
        .unwrap()
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn railway_style_capitalised_references_resolve(pool: PgPool) {
    use secrecy::ExposeSecret;
    let f = fixture(pool).await;
    let pg = f.template_service("postgres").await;
    let r = f
        .set_vars(
            &f.admin,
            json!({ "set": { "DATABASE_URL": "${{Postgres.DATABASE_URL}}?schema=public" } }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let resolved = f.resolve(f.production().await, &f.instance).await;
    assert!(resolved.missing.is_empty(), "{:?}", resolved.missing);
    let url = resolved.vars["DATABASE_URL"].expose_secret().to_owned();
    assert!(
        url.starts_with("postgresql://postgres:")
            && url.ends_with("@postgres.modula.internal:5432/app?schema=public"),
        "{url}"
    );

    // The canvas edge comes from the stored (normalized) reference.
    let p = f
        .app
        .call(
            Method::GET,
            &format!("/api/projects/{}", f.project),
            Some(&f.admin),
            None,
        )
        .await;
    let api = p.json["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == f.instance.as_str())
        .unwrap();
    assert_eq!(api["dependsOn"], json!([pg["serviceId"]]));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn cloned_environments_get_volumes_and_fresh_database_passwords(pool: PgPool) {
    use secrecy::ExposeSecret;
    let f = fixture(pool).await;
    let pg = f.template_service("postgres").await;
    let prod = f.production().await;
    f.set_vars(&f.admin, json!({ "set": { "PLAIN": "1" } }))
        .await;
    let db = f.app.state.db.clone();
    let keys = &f.app.state.keys;
    let password = |env: Id| {
        let db = db.clone();
        async move {
            let (id, sealed): (Id, Vec<u8>) = sqlx::query_as(
                "SELECT v.id, v.value_sealed FROM variables v
                 JOIN service_instances i ON i.id = v.service_instance_id
                 JOIN services s ON s.id = i.service_id
                 WHERE v.environment_id = $1 AND s.name = 'postgres' AND v.name = 'POSTGRES_PASSWORD'",
            )
            .bind(env)
            .fetch_one(&db)
            .await
            .unwrap();
            modula_server::deploy::env::open_variable(keys, &db, env, id, &sealed)
                .await
                .unwrap()
                .expose_secret()
                .to_owned()
        }
    };
    let prod_password = password(prod).await;

    for duplicate in [false, true] {
        let name = format!("clone-{duplicate}");
        let body = if duplicate {
            json!({ "name": name, "duplicateFrom": prod })
        } else {
            json!({ "name": name })
        };
        let r = f
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{}/environments", f.project),
                Some(&f.admin),
                Some(body),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        let env: Id = r.json["id"].as_str().unwrap().parse().unwrap();
        let instance: String = sqlx::query_scalar(
            "SELECT i.id FROM service_instances i JOIN services s ON s.id = i.service_id
             WHERE i.environment_id = $1 AND s.name = 'postgres'",
        )
        .bind(env)
        .fetch_one(&db)
        .await
        .unwrap();

        // A volume of its own at the same path; no public exposure.
        let (volume, mount): (String, String) =
            sqlx::query_as("SELECT id, mount_path FROM volumes WHERE service_instance_id = $1")
                .bind(&instance)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_ne!(volume, pg["volume"]["id"].as_str().unwrap());
        assert_eq!(mount, pg["volume"]["mountPath"].as_str().unwrap());
        let exposed: i64 = sqlx::query_scalar(
            "SELECT (SELECT count(*) FROM domains WHERE service_instance_id = $1)
                  + (SELECT count(*) FROM tcp_proxies WHERE service_instance_id = $1)",
        )
        .bind(&instance)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(exposed, 0);

        // The database works: every template variable, a new password.
        let fresh = password(env).await;
        assert_eq!(fresh.len(), 32);
        assert_ne!(fresh, prod_password, "duplicate={duplicate}");
        let resolved = f.resolve(env, &instance).await;
        assert!(resolved.missing.is_empty(), "{:?}", resolved.missing);
        assert!(
            resolved.vars["DATABASE_URL"]
                .expose_secret()
                .contains(&format!(":{fresh}@postgres.modula.internal:5432/app"))
        );
        let plain: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM variables WHERE environment_id = $1 AND name = 'PLAIN'",
        )
        .bind(env)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(plain, i64::from(duplicate));
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn public_domain_variable_skips_unverified_custom_domains(pool: PgPool) {
    use secrecy::ExposeSecret;
    let f = fixture(pool).await;
    let prod = f.production().await;
    f.set_vars(
        &f.admin,
        json!({ "set": { "PUB": "${{MODULA_PUBLIC_DOMAIN}}" } }),
    )
    .await;
    let public = || async {
        f.resolve(prod, &f.instance).await.vars["PUB"]
            .expose_secret()
            .to_owned()
    };

    // A custom domain that hasn't been verified is not served.
    sqlx::query(
        "INSERT INTO domains (id, service_instance_id, hostname, kind, status, verification_token, created_at)
         VALUES ($1, $2, 'shop.example.com', 'custom', 'pending', 'token', now() - interval '1 hour')",
    )
    .bind(Id::new())
    .bind(&f.instance)
    .execute(&f.app.state.db)
    .await
    .unwrap();
    assert_eq!(public().await, "");

    sqlx::query(
        "INSERT INTO domains (id, service_instance_id, hostname, kind)
         VALUES ($1, $2, 'api-shop.modula.app', 'generated')",
    )
    .bind(Id::new())
    .bind(&f.instance)
    .execute(&f.app.state.db)
    .await
    .unwrap();
    assert_eq!(public().await, "api-shop.modula.app");
}

impl Fixture {
    /// A server to place deployments on (it never connects in these tests).
    async fn add_node(&self) {
        sqlx::query(
            "INSERT INTO nodes (id, workspace_id, name, cert_serial, cert_expires_at)
             VALUES ($1, $2, 'worker', $3, now() + interval '1 day')",
        )
        .bind(Id::new())
        .bind(self.workspace)
        .bind(Id::new().to_string())
        .execute(&self.app.state.db)
        .await
        .unwrap();
    }

    async fn raw_vars(&self) -> modula_server::deploy::env::RawVars {
        modula_server::deploy::env::raw_variables(
            &self.app.state.keys,
            &self.app.state.db,
            self.production().await,
            self.instance.parse().unwrap(),
        )
        .await
        .unwrap()
    }

    async fn pending(&self, cookie: &str, query: &str) -> Value {
        let env = self.production().await;
        let r = self
            .app
            .call(
                Method::GET,
                &format!("/api/environments/{env}/pending{query}"),
                Some(cookie),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json
    }

    async fn discard(&self, cookie: &str, changes: Option<Value>) -> Resp {
        let env = self.production().await;
        let service: Id =
            sqlx::query_scalar("SELECT service_id FROM service_instances WHERE id = $1")
                .bind(&self.instance)
                .fetch_one(&self.app.state.db)
                .await
                .unwrap();
        let mut body = json!({ "serviceId": service });
        if let Some(c) = changes {
            body["changes"] = c;
        }
        self.app
            .call(
                Method::POST,
                &format!("/api/environments/{env}/discard"),
                Some(cookie),
                Some(body),
            )
            .await
    }
}

/// `kind op key` lines of a pending response.
fn pending_lines(p: &Value) -> Vec<String> {
    let mut lines: Vec<String> = p["services"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| s["changes"].as_array().unwrap().clone())
        .map(|c| {
            format!(
                "{} {} {}",
                c["kind"].as_str().unwrap(),
                c["op"].as_str().unwrap(),
                c["key"].as_str().unwrap()
            )
        })
        .collect();
    lines.sort();
    lines
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn pending_changes_diff_against_the_deployment_and_can_be_discarded(pool: PgPool) {
    let f = fixture(pool).await;
    f.add_node().await;
    f.set_vars(
        &f.admin,
        json!({ "set": { "KEEP": "1", "CHANGE": "old", "DROP": "gone", "REF": "${{KEEP}}-x", "SECRET": "s3cret-old" },
                "sealed": ["SECRET"] }),
    )
    .await;
    let env = f.production().await;
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/environments/{env}/deploy"),
            Some(&f.admin),
            Some(json!({ "message": "  first release  " })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let deployment = r.json["deployments"][0]["deploymentId"]
        .as_str()
        .unwrap()
        .to_owned();
    let d = f
        .app
        .call(
            Method::GET,
            &format!("/api/deployments/{deployment}"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(
        d.json["message"], "first release",
        "the deploy message is stored trimmed"
    );
    let feed = f
        .app
        .call(
            Method::GET,
            &format!("/api/projects/{}/activity?kind=deployment", f.project),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(
        feed.json["items"][0]["changes"][0]["message"],
        "first release"
    );
    assert_eq!(f.pending(&f.admin, "").await["count"], 0);

    // Edit everything, including a key changed and changed back.
    f.set_vars(
        &f.admin,
        json!({ "set": { "ADDED": "new", "CHANGE": "new", "REF": "plain", "SECRET": "s3cret-new", "KEEP": "2" },
                "delete": ["DROP"] }),
    )
    .await;
    f.set_vars(&f.admin, json!({ "set": { "KEEP": "1" } }))
        .await;
    let r = f
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{}", f.instance),
            Some(&f.admin),
            Some(json!({ "config": { "source": { "type": "image", "image": "nginx:alpine" }, "port": 3000 } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let p = f.pending(&f.admin, "?values=true").await;
    assert_eq!(
        pending_lines(&p),
        [
            "config updated port",
            "variable added ADDED",
            "variable removed DROP",
            "variable updated CHANGE",
            "variable updated REF",
            "variable updated SECRET",
        ],
        "KEEP is back to its deployed value: {p:?}"
    );
    assert_eq!(p["count"], 6);
    let find = |p: &Value, key: &str| {
        p["services"][0]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["key"] == key)
            .unwrap()
            .clone()
    };
    let change = find(&p, "CHANGE");
    assert_eq!(
        (change["currentValue"].as_str(), change["newValue"].as_str()),
        (Some("old"), Some("new"))
    );
    assert_eq!(
        find(&p, "REF")["currentValue"],
        "${{KEEP}}-x",
        "values as written, not resolved"
    );
    let secret = find(&p, "SECRET");
    assert_eq!(secret["sealed"], true);
    assert!(secret["currentValue"].is_null() && secret["newValue"].is_null());
    assert!(
        !p.to_string().contains("s3cret"),
        "sealed values never leave the server"
    );
    assert_eq!(find(&p, "port")["newValue"], "3000");

    // Values are for Members and up; Deployers can't discard.
    let (viewer_id, viewer) = f.app.user("viewer@example.com").await;
    f.app.add_member(f.workspace, viewer_id, "viewer").await;
    let (deployer_id, deployer) = f.app.user("deployer@example.com").await;
    f.app.add_member(f.workspace, deployer_id, "deployer").await;
    let seen = f.pending(&viewer, "?values=true").await;
    assert_eq!(seen["count"], 6);
    assert!(find(&seen, "CHANGE")["currentValue"].is_null());
    assert_eq!(
        f.discard(&deployer, None).await.status,
        StatusCode::FORBIDDEN
    );

    // One change: CHANGE gets its deployed value back.
    let r = f
        .discard(
            &f.admin,
            Some(json!([{ "kind": "variable", "key": "CHANGE" }])),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["count"], 5);
    assert_eq!(f.raw_vars().await["CHANGE"].value, "old");
    let r = f
        .discard(
            &f.admin,
            Some(json!([{ "kind": "variable", "key": "CHANGE" }])),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "nothing left to discard");

    // The rest: added removed, deleted restored, sealed restored still sealed.
    let r = f.discard(&f.admin, None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["count"], 0, "{:?}", r.json);
    assert!(!r.json.to_string().contains("s3cret"));
    let vars = f.raw_vars().await;
    assert!(!vars.contains_key("ADDED"));
    assert_eq!(vars["DROP"].value, "gone");
    assert_eq!(
        vars["REF"].value, "${{KEEP}}-x",
        "the reference itself is restored"
    );
    assert_eq!(vars["SECRET"].value, "s3cret-old");
    assert!(vars["SECRET"].sealed);
    let cfg: Value = sqlx::query_scalar("SELECT config FROM service_instances WHERE id = $1")
        .bind(&f.instance)
        .fetch_one(&f.app.state.db)
        .await
        .unwrap();
    let deployed: Value = sqlx::query_scalar("SELECT config FROM deployments WHERE id = $1")
        .bind(&deployment)
        .fetch_one(&f.app.state.db)
        .await
        .unwrap();
    assert_eq!(
        cfg.get("port"),
        deployed.get("port"),
        "the setting is restored"
    );
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'pending.discard'")
            .fetch_one(&f.app.state.db)
            .await
            .unwrap();
    assert_eq!(audited, 2);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn renaming_a_service_rewrites_references_everywhere(pool: PgPool) {
    use secrecy::ExposeSecret;
    let f = fixture(pool).await;
    f.add_node().await;
    let pg = f.template_service("postgres").await;
    let pg_id = pg["serviceId"].as_str().unwrap().to_owned();
    let production = f.production().await;
    let r = f
        .set_vars(
            &f.admin,
            json!({ "set": {
                "DATABASE_URL": "${{ Postgres.DATABASE_URL }}?schema=public",
                "MIXED": "${{postgres.PGHOST}}:${{shared.PORT}} postgres.PGHOST ${{api.KEEP}} ${{postgres-1.X}}",
                "KEEP": "postgres",
                "POSTGRES_URL": "plain",
            } }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let call = |method: Method, path: String, body: Option<Value>| {
        let f = &f;
        async move { f.app.call(method, &path, Some(&f.admin), body).await }
    };
    let r = call(
        Method::POST,
        format!("/api/projects/{}/environments", f.project),
        Some(json!({ "name": "staging" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let staging = r.json["id"].as_str().unwrap().to_owned();
    let r = call(
        Method::PATCH,
        format!("/api/environments/{staging}/variables"),
        Some(json!({ "set": { "PG": "${{postgres.DATABASE_URL}}", "PORT": "5432" } })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // Deployed in production: the rewrite shows up as pending changes there.
    let r = call(
        Method::POST,
        format!("/api/environments/{production}/deploy"),
        Some(json!({})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(f.pending(&f.admin, "").await["count"], 0);

    let preview = call(
        Method::GET,
        format!("/api/services/{pg_id}/rename-preview"),
        None,
    )
    .await;
    assert_eq!(preview.status, StatusCode::OK, "{:?}", preview.json);
    let names = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{}/{}",
                    r["serviceName"].as_str().unwrap_or("shared"),
                    r["name"].as_str().unwrap()
                )
            })
            .collect()
    };
    assert_eq!(
        names(&preview.json["references"]),
        ["api/DATABASE_URL", "api/MIXED", "shared/PG"]
    );
    assert!(names(&preview.json["similarNames"]).contains(&"api/POSTGRES_URL".to_owned()));

    let rename = |name: &'static str| {
        call(
            Method::PATCH,
            format!("/api/services/{pg_id}"),
            Some(json!({ "name": name })),
        )
    };
    assert_eq!(rename("api").await.status, StatusCode::CONFLICT);
    for bad in ["Postgres API", "shared", "-db", ""] {
        assert_eq!(rename(bad).await.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let r = rename("postgres-api").await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["rewrittenVariables"], 3);

    // Only references to the service changed; other text and references stay.
    let raw = f.raw_vars().await;
    assert_eq!(
        raw["DATABASE_URL"].value,
        "${{postgres-api.DATABASE_URL}}?schema=public"
    );
    assert_eq!(
        raw["MIXED"].value,
        "${{postgres-api.PGHOST}}:${{shared.PORT}} postgres.PGHOST ${{api.KEEP}} ${{postgres-1.X}}"
    );
    assert_eq!(raw["KEEP"].value, "postgres");
    assert!(
        raw.contains_key("POSTGRES_URL"),
        "variable names are untouched"
    );
    let resolved = f.resolve(production, &f.instance).await;
    let url = resolved.vars["DATABASE_URL"].expose_secret().to_owned();
    assert!(
        url.ends_with("@postgres-api.modula.internal:5432/app?schema=public"),
        "{url}"
    );
    // The shared variable in the other environment, with its stored refs.
    let shared = call(
        Method::GET,
        format!("/api/environments/{staging}/variables"),
        None,
    )
    .await;
    let pg_var = shared
        .json
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "PG")
        .unwrap();
    assert_eq!(pg_var["references"], json!(["postgres-api.DATABASE_URL"]));

    // Pending: the referencing service's variables, and the renamed service.
    assert_eq!(
        pending_lines(&f.pending(&f.admin, "").await),
        [
            "service updated name",
            "variable updated DATABASE_URL",
            "variable updated MIXED",
        ]
    );
    let audit: Value =
        sqlx::query_scalar("SELECT metadata FROM audit_log WHERE action = 'service.rename'")
            .fetch_one(&f.app.state.db)
            .await
            .unwrap();
    assert_eq!(
        audit,
        json!({ "from": "postgres", "to": "postgres-api", "rewrittenVariables": 3 })
    );
    // The old name stays resolvable for deployments from before the rename.
    let aliases: Vec<String> = sqlx::query_scalar("SELECT name FROM service_aliases")
        .fetch_all(&f.app.state.db)
        .await
        .unwrap();
    assert_eq!(aliases, ["postgres"]);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_new_service_waits_in_pending_changes_until_deployed(pool: PgPool) {
    let f = fixture(pool).await;
    f.add_node().await;
    f.set_vars(
        &f.admin,
        json!({ "set": { "SECRET": "s3cret" }, "sealed": ["SECRET"] }),
    )
    .await;
    let repo = f
        .app
        .call(
            Method::POST,
            &format!("/api/projects/{}/services", f.project),
            Some(&f.admin),
            Some(json!({ "name": "web", "config": {
                "source": { "type": "repo", "repo": "acme/shop", "branch": "main" },
                "buildCommand": "pnpm --filter web build",
            } })),
        )
        .await;
    assert_eq!(repo.status, StatusCode::OK, "{:?}", repo.json);
    let web = repo.json["id"].as_str().unwrap().to_owned();

    // Nothing deployed: both services are new, with their whole setup listed.
    let p = f.pending(&f.admin, "?values=true").await;
    let services = p["services"].as_array().unwrap();
    assert!(
        services
            .iter()
            .all(|s| s["new"] == true && s["removable"] == true),
        "{p:?}"
    );
    let web_changes = &services
        .iter()
        .find(|s| s["serviceId"] == web.as_str())
        .unwrap()["changes"];
    let keys: Vec<_> = web_changes
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{} {}",
                c["kind"].as_str().unwrap(),
                c["key"].as_str().unwrap_or("-")
            )
        })
        .collect();
    assert_eq!(
        keys,
        [
            "service -",
            "config repo",
            "config branch",
            "config buildCommand",
            "config builder"
        ],
        "source fields on their own, defaults (port, replicas…) left out but the builder kept"
    );
    assert!(
        !p.to_string().contains("s3cret"),
        "sealed values stay sealed"
    );
    let api = services
        .iter()
        .find(|s| s["serviceId"] != web.as_str())
        .unwrap();
    assert!(
        api["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "variable" && c["key"] == "SECRET"),
        "{api:?}"
    );

    // Discarding a never-deployed service removes it.
    let env = f.production().await;
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/environments/{env}/discard"),
            Some(&f.admin),
            Some(json!({ "serviceId": web })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM services WHERE id = $1")
        .bind(web.parse::<Id>().unwrap())
        .fetch_one(&f.app.state.db)
        .await
        .unwrap();
    assert_eq!(left, 0);

    // Deploying the changes creates the rest; then nothing is pending.
    let r = f
        .app
        .call(
            Method::POST,
            &format!("/api/environments/{env}/deploy"),
            Some(&f.admin),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["deployments"].as_array().unwrap().len(), 1);
    assert_eq!(f.pending(&f.admin, "").await["count"], 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_detected_start_command_stays_detected_until_edited(pool: PgPool) {
    let f = fixture(pool).await;
    let s = f
        .app
        .call(
            Method::POST,
            &format!("/api/projects/{}/services", f.project),
            Some(&f.admin),
            Some(json!({ "name": "web", "config": {
                "source": { "type": "repo", "repo": "acme/shop", "branch": "main" },
                "builder": "railpack",
                "startCommand": ["pnpm", "--filter", "web", "start"],
                "startCommandDetected": true,
            } })),
        )
        .await;
    assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
    let instance = s.json["instances"][0]["id"].as_str().unwrap().to_owned();
    // The dashboard sends whole configs without the flag; the server keeps it.
    let patch = |config: Value| {
        let (f, instance) = (&f, instance.clone());
        async move {
            let r = f
                .app
                .call(
                    Method::PATCH,
                    &format!("/api/service-instances/{instance}"),
                    Some(&f.admin),
                    Some(json!({ "config": config })),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
            r.json["config"].clone()
        }
    };
    let source = json!({ "type": "repo", "repo": "acme/shop", "branch": "main" });
    let c = patch(
        json!({ "source": source, "builder": "dockerfile", "port": 3000,
                          "startCommand": ["pnpm", "--filter", "web", "start"] }),
    )
    .await;
    assert_eq!(
        c["startCommandDetected"], true,
        "unchanged start command: {c}"
    );
    let c = patch(json!({ "source": source, "builder": "dockerfile",
                          "startCommand": ["node", "server.js"] }))
    .await;
    assert!(
        c.get("startCommandDetected").is_none(),
        "edited: the user's now: {c}"
    );
    // Not a setting of its own in the activity feed.
    let feed = f
        .app
        .call(
            Method::GET,
            &format!("/api/projects/{}/activity?kind=config", f.project),
            Some(&f.admin),
            None,
        )
        .await;
    assert!(
        !feed.json.to_string().contains("startCommandDetected"),
        "{:?}",
        feed.json
    );
}
