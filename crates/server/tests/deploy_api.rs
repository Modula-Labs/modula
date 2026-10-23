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
        .set_vars(&f.admin, json!({ "set": { "DATABASE_URL": "postgres://u:hunter2@db/x", "STRIPE_KEY": "sk_live_123456" }, "sealed": ["STRIPE_KEY"] }))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let listed = r.json.to_string();
    assert!(
        !listed.contains("hunter2") && !listed.contains("sk_live"),
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
        json!({ "set": { "STRIPE_KEY": "sk_live_rotated" } }),
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
        !all.contains("hunter2") && !all.contains("sk_live"),
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

    // Deleting an environment destroys its data key; the last one can't go.
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
async fn environments_clone_config_and_optionally_variables(pool: PgPool) {
    let f = fixture(pool).await;
    f.set_vars(
        &f.admin,
        json!({ "set": { "PLAIN": "1", "SECRET": "2" }, "sealed": ["SECRET"] }),
    )
    .await;
    let prod: String =
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE id = $1")
            .bind(&f.instance)
            .fetch_one(&f.app.state.db)
            .await
            .unwrap();
    let create = |body: Value| {
        let (app, admin, project) = (&f.app, f.admin.clone(), f.project.clone());
        async move {
            let r = app
                .call(
                    Method::POST,
                    &format!("/api/projects/{project}/environments"),
                    Some(&admin),
                    Some(body),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
            r.json["id"].as_str().unwrap().to_owned()
        }
    };
    let names = |env: String| {
        let db = f.app.state.db.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT name FROM variables WHERE environment_id = $1 ORDER BY name",
            )
            .bind(env)
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };
    let bare = create(json!({ "name": "bare", "cloneFrom": prod })).await;
    assert!(names(bare).await.is_empty());
    let copy = create(json!({ "name": "copy", "cloneFrom": prod, "copyVariables": true })).await;
    assert_eq!(
        names(copy).await,
        ["PLAIN"],
        "sealed variables are never copied"
    );
}
