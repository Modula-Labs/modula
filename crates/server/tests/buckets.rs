//! Buckets: Cloudflare R2 buckets (against an in-process mock of the v4 API:
//! buckets, permission groups and account-owned tokens) and the self-hosted
//! bucket template.

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::IntoResponse;
use common::*;
use modula_id::Id;
use secrecy::ExposeSecret;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const TOKEN: &str = "cf-test-token-0123456789abcdefWXYZ";
const READ_GROUP: &str = "pg-r2-item-read";
const WRITE_GROUP: &str = "pg-r2-item-write";

#[derive(Default)]
struct Cf {
    /// Bucket name → whether it holds objects.
    buckets: Mutex<BTreeMap<String, bool>>,
    /// Account-owned tokens: id → (value, request body).
    tokens: Mutex<BTreeMap<String, (String, Value)>>,
    tokens_allowed: AtomicBool,
    next: AtomicU32,
}

impl Cf {
    fn buckets(&self) -> Vec<String> {
        self.buckets.lock().unwrap().keys().cloned().collect()
    }
    fn token_ids(&self) -> Vec<String> {
        self.tokens.lock().unwrap().keys().cloned().collect()
    }
}

fn ok(result: Value) -> axum::response::Response {
    axum::Json(json!({ "success": true, "errors": [], "messages": [], "result": result }))
        .into_response()
}

fn fail(status: StatusCode, code: i64, message: &str) -> axum::response::Response {
    (
        status,
        axum::Json(json!({ "success": false, "errors": [{ "code": code, "message": message }], "result": null })),
    )
        .into_response()
}

async fn handle(
    State(cf): State<Arc<Cf>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {TOKEN}"))
    {
        return fail(StatusCode::UNAUTHORIZED, 1000, "Invalid API Token");
    }
    let path = uri.path().trim_start_matches("/client/v4/").to_owned();
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let seg: Vec<&str> = path.split('/').collect();
    let tokens_allowed = cf.tokens_allowed.load(Ordering::SeqCst);
    match (method.as_str(), &seg[..]) {
        ("GET", ["user", "tokens", "verify"]) => ok(json!({ "id": "tok1", "status": "active" })),
        ("GET", ["accounts"]) => ok(json!([{ "id": "acc1", "name": "Acme" }])),
        ("GET", ["zones"]) => ok(json!([])),
        ("GET", ["accounts", "acc1", "r2", "buckets"]) => {
            let list: Vec<Value> = cf
                .buckets()
                .into_iter()
                .map(|n| json!({ "name": n, "creation_date": "2026-01-01T00:00:00Z", "location": "WEUR" }))
                .collect();
            ok(json!({ "buckets": list }))
        }
        ("POST", ["accounts", "acc1", "r2", "buckets"]) => {
            let name = body["name"].as_str().unwrap().to_owned();
            let mut b = cf.buckets.lock().unwrap();
            if b.contains_key(&name) {
                return fail(StatusCode::CONFLICT, 10004, "The bucket already exists");
            }
            b.insert(name.clone(), false);
            ok(json!({ "name": name }))
        }
        ("DELETE", ["accounts", "acc1", "r2", "buckets", name]) => {
            let mut b = cf.buckets.lock().unwrap();
            match b.get(*name) {
                None => fail(StatusCode::NOT_FOUND, 10006, "The bucket does not exist"),
                Some(true) => fail(StatusCode::CONFLICT, 10008, "The bucket is not empty"),
                Some(false) => {
                    b.remove(*name);
                    ok(json!({}))
                }
            }
        }
        (_, ["accounts", "acc1", "tokens", ..]) if !tokens_allowed => fail(
            StatusCode::FORBIDDEN,
            9109,
            "Unauthorized to access requested resource",
        ),
        ("GET", ["accounts", "acc1", "tokens", "permission_groups"]) => ok(json!([
            { "id": "pg-zone-read", "name": "Zone Read", "scopes": ["com.cloudflare.api.account.zone"] },
            { "id": READ_GROUP, "name": "Workers R2 Storage Bucket Item Read", "scopes": ["com.cloudflare.edge.r2.bucket"] },
            { "id": WRITE_GROUP, "name": "Workers R2 Storage Bucket Item Write", "scopes": ["com.cloudflare.edge.r2.bucket"] },
        ])),
        ("POST", ["accounts", "acc1", "tokens"]) => {
            let n = cf.next.fetch_add(1, Ordering::SeqCst);
            let id = format!("token{n}");
            let value = format!("token-value-{n}");
            cf.tokens
                .lock()
                .unwrap()
                .insert(id.clone(), (value.clone(), body.clone()));
            ok(json!({ "id": id, "name": body["name"], "status": "active", "value": value }))
        }
        ("DELETE", ["accounts", "acc1", "tokens", id]) => {
            match cf.tokens.lock().unwrap().remove(*id) {
                Some(_) => ok(json!({ "id": id })),
                None => fail(StatusCode::NOT_FOUND, 1003, "Token not found"),
            }
        }
        _ => fail(StatusCode::NOT_FOUND, 7003, "Could not route"),
    }
}

async fn app_with_cf(pool: PgPool) -> (TestApp, Arc<Cf>) {
    let cf = Arc::new(Cf::default());
    let router = axum::Router::new().fallback(handle).with_state(cf.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut cfg = config();
    cfg.cloudflare_api = format!("http://{addr}/client/v4").parse().unwrap();
    (TestApp::with_config(pool, cfg), cf)
}

struct Ctx {
    app: TestApp,
    cf: Arc<Cf>,
    admin: String,
    ws: Id,
    project: String,
    prod: Id,
    staging: Id,
}

impl Ctx {
    async fn new(pool: PgPool) -> Self {
        let (app, cf) = app_with_cf(pool).await;
        let admin = app.setup_admin().await;
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
        let project = r.json["id"].as_str().unwrap().to_owned();
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
        let prod: Id =
            sqlx::query_scalar("SELECT id FROM environments WHERE project_id = $1 AND id <> $2")
                .bind(&project)
                .bind(staging)
                .fetch_one(app.db())
                .await
                .unwrap();
        Self {
            app,
            cf,
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

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let r = self.call(method.clone(), path, body).await;
        assert_eq!(r.status, StatusCode::OK, "{method} {path}: {:?}", r.json);
        r.json
    }

    async fn connect(&self) {
        self.ok(
            Method::PUT,
            &format!("/api/workspaces/{}/integrations/cloudflare", self.ws),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    }

    /// The instance of `service` in `env`.
    async fn instance(&self, service: &str, env: Id) -> Id {
        sqlx::query_scalar(
            "SELECT i.id FROM service_instances i JOIN services s ON s.id = i.service_id
             WHERE s.name = $1 AND i.environment_id = $2",
        )
        .bind(service)
        .bind(env)
        .fetch_one(self.app.db())
        .await
        .unwrap()
    }

    /// Variable name → sealed, for an instance.
    async fn variables(&self, instance: Id) -> BTreeMap<String, bool> {
        let r = self
            .ok(
                Method::GET,
                &format!("/api/service-instances/{instance}/variables"),
                None,
            )
            .await;
        r.as_array()
            .unwrap()
            .iter()
            .map(|v| {
                (
                    v["name"].as_str().unwrap().to_owned(),
                    v["sealed"].as_bool().unwrap(),
                )
            })
            .collect()
    }

    /// What a service's deployment would get, resolved.
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

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn r2_buckets_get_their_own_scoped_credentials(pool: PgPool) {
    let c = Ctx::new(pool).await;
    let create = format!("/api/projects/{}/buckets", c.project);
    let body = |bucket: &str| json!({ "name": "media", "environmentId": c.prod, "bucket": bucket });

    // Without Cloudflare there is nothing to create buckets with.
    let r = c
        .call(Method::POST, &create, Some(body("media-prod")))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(
        r.json.to_string().contains("connect Cloudflare"),
        "{:?}",
        r.json
    );
    c.connect().await;

    // A token that can't mint tokens: the error names the permission, and the
    // bucket made on the way is removed again, as is the service.
    let r = c
        .call(Method::POST, &create, Some(body("media-prod")))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    assert!(
        r.json.to_string().contains("Account API Tokens"),
        "{:?}",
        r.json
    );
    assert!(c.cf.buckets().is_empty());
    let services: i64 = sqlx::query_scalar("SELECT count(*) FROM services")
        .fetch_one(c.app.db())
        .await
        .unwrap();
    assert_eq!(services, 0);
    c.cf.tokens_allowed.store(true, Ordering::SeqCst);

    // Invalid names never reach Cloudflare.
    let r = c
        .call(Method::POST, &create, Some(body("Media_Prod")))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    let created = c.ok(Method::POST, &create, Some(body("media-prod"))).await;
    assert_eq!(created["instances"].as_array().unwrap().len(), 2);
    assert_eq!(c.cf.buckets(), ["media-prod"]);
    let prod = c.instance("media", c.prod).await;
    let staging = c.instance("media", c.staging).await;
    let inst = c
        .ok(Method::GET, &format!("/api/service-instances/{prod}"), None)
        .await;
    assert_eq!(inst["template"], "r2");
    assert_eq!(inst["config"]["source"]["type"], "r2");
    assert_eq!(inst["r2"]["bucket"], "media-prod");
    assert_eq!(inst["r2"]["created"], true);
    let other = c
        .ok(
            Method::GET,
            &format!("/api/service-instances/{staging}"),
            None,
        )
        .await;
    assert!(
        other["r2"].is_null(),
        "only the chosen environment gets a bucket"
    );

    // The token may only touch this bucket's objects.
    let (token_id, (value, req)) =
        c.cf.tokens
            .lock()
            .unwrap()
            .iter()
            .next()
            .map(|(k, v)| (k.clone(), v.clone()))
            .unwrap();
    let policy = &req["policies"][0];
    assert_eq!(policy["effect"], "allow");
    assert_eq!(
        policy["resources"],
        json!({ "com.cloudflare.edge.r2.bucket.acc1_default_media-prod": "*" })
    );
    let mut groups: Vec<&str> = policy["permission_groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["id"].as_str().unwrap())
        .collect();
    groups.sort();
    assert_eq!(groups, [READ_GROUP, WRITE_GROUP]);

    // The S3 credentials are variables; the secret is sealed.
    let vars = c.variables(prod).await;
    for name in [
        "S3_BUCKET",
        "BUCKET_NAME",
        "S3_ENDPOINT",
        "S3_REGION",
        "S3_FORCE_PATH_STYLE",
        "AWS_ACCESS_KEY_ID",
    ] {
        assert_eq!(vars.get(name), Some(&false), "{name}: {vars:?}");
    }
    assert_eq!(vars.get("AWS_SECRET_ACCESS_KEY"), Some(&true));
    assert!(c.variables(staging).await.is_empty());

    // Other services reference them like any service's variables.
    let web = c
        .ok(
            Method::POST,
            &format!("/api/projects/{}/services", c.project),
            Some(json!({ "name": "web", "config": { "source": { "type": "image", "image": "app:1" }, "port": 8080 } })),
        )
        .await;
    let web_prod = c.instance("web", c.prod).await;
    assert!(web["id"].is_string());
    c.ok(
        Method::PATCH,
        &format!("/api/service-instances/{web_prod}/variables"),
        Some(json!({ "set": {
            "BUCKET": "${{media.BUCKET_NAME}}",
            "ENDPOINT": "${{media.S3_ENDPOINT}}",
            "REGION": "${{media.S3_REGION}}",
            "KEY": "${{media.AWS_ACCESS_KEY_ID}}",
            "SECRET": "${{media.AWS_SECRET_ACCESS_KEY}}",
        } })),
    )
    .await;
    let env = c.resolved(web_prod, c.prod).await;
    assert_eq!(env["BUCKET"], "media-prod");
    assert_eq!(env["ENDPOINT"], "https://acc1.r2.cloudflarestorage.com");
    assert_eq!(env["REGION"], "auto");
    assert_eq!(env["KEY"], token_id);
    assert_eq!(env["SECRET"], hex::encode(Sha256::digest(value.as_bytes())));

    // Nothing to deploy, configure or count as pending.
    let r = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{prod}/deployments"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = c
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{prod}"),
            Some(json!({ "config": { "source": { "type": "image", "image": "app:1" } } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let pending = c
        .ok(
            Method::GET,
            &format!("/api/environments/{}/pending", c.prod),
            None,
        )
        .await;
    assert!(
        pending["services"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["serviceName"] != "media"),
        "{pending:?}"
    );

    // A name already taken in the account is linked, not adopted.
    c.cf.buckets.lock().unwrap().insert("legacy".into(), true);
    let r = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{staging}/r2"),
            Some(json!({ "bucket": "legacy" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    let r = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{staging}/r2"),
            Some(json!({ "existing": "nope" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let linked = c
        .ok(
            Method::POST,
            &format!("/api/service-instances/{staging}/r2"),
            Some(json!({ "existing": "legacy" })),
        )
        .await;
    assert_eq!(linked["r2"]["bucket"], "legacy");
    assert_eq!(linked["r2"]["created"], false);
    assert_eq!(c.cf.token_ids().len(), 2);

    // Settings lists the account's buckets and what uses them.
    let list = c
        .ok(
            Method::GET,
            &format!(
                "/api/workspaces/{}/integrations/cloudflare/r2/buckets",
                c.ws
            ),
            None,
        )
        .await;
    let by_name: BTreeMap<&str, &Value> = list["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| (b["name"].as_str().unwrap(), b))
        .collect();
    assert_eq!(
        by_name["media-prod"]["links"][0]["environmentName"],
        "production"
    );
    assert_eq!(by_name["legacy"]["links"][0]["environmentName"], "staging");
    assert_eq!(by_name["legacy"]["links"][0]["serviceName"], "media");

    // A linked bucket is never deleted from here.
    let delete = |i: Id| format!("/api/service-instances/{i}/r2/delete-bucket");
    let r = c
        .call(
            Method::POST,
            &delete(staging),
            Some(json!({ "confirm": "legacy" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);

    // Deleting a created one takes its exact name, and Cloudflare only
    // deletes empty buckets: a refusal changes nothing.
    let r = c
        .call(
            Method::POST,
            &delete(prod),
            Some(json!({ "confirm": "media" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    c.cf.buckets
        .lock()
        .unwrap()
        .insert("media-prod".into(), true);
    let r = c
        .call(
            Method::POST,
            &delete(prod),
            Some(json!({ "confirm": "media-prod" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(r.json.to_string().contains("empty"), "{:?}", r.json);
    assert!(c.cf.token_ids().contains(&token_id));
    c.cf.buckets
        .lock()
        .unwrap()
        .insert("media-prod".into(), false);
    c.ok(
        Method::POST,
        &delete(prod),
        Some(json!({ "confirm": "media-prod" })),
    )
    .await;
    assert_eq!(c.cf.buckets(), ["legacy"]);
    assert!(!c.cf.token_ids().contains(&token_id));
    assert!(c.variables(prod).await.is_empty());
    let inst = c
        .ok(Method::GET, &format!("/api/service-instances/{prod}"), None)
        .await;
    assert!(inst["r2"].is_null());

    // Unlinking revokes the token and keeps the bucket.
    c.ok(
        Method::DELETE,
        &format!("/api/service-instances/{staging}/r2"),
        None,
    )
    .await;
    assert!(c.cf.token_ids().is_empty());
    assert_eq!(c.cf.buckets(), ["legacy"]);
    assert!(c.variables(staging).await.is_empty());

    // Deleting the service revokes its tokens; the buckets (data) stay.
    c.ok(
        Method::POST,
        &format!("/api/service-instances/{prod}/r2"),
        Some(json!({ "bucket": "media-two" })),
    )
    .await;
    assert_eq!(c.cf.token_ids().len(), 1);
    c.ok(
        Method::DELETE,
        &format!(
            "/api/services/{}?allEnvironments=true",
            created["id"].as_str().unwrap()
        ),
        None,
    )
    .await;
    assert!(c.cf.token_ids().is_empty());
    assert_eq!(c.cf.buckets(), ["legacy", "media-two"]);

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'bucket.%' ORDER BY id")
            .fetch_all(c.app.db())
            .await
            .unwrap();
    assert_eq!(
        actions,
        [
            "bucket.provision",
            "bucket.provision",
            "bucket.delete",
            "bucket.unlink",
            "bucket.provision"
        ]
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn deleting_an_environment_revokes_its_bucket_token(pool: PgPool) {
    let c = Ctx::new(pool).await;
    c.connect().await;
    c.cf.tokens_allowed.store(true, Ordering::SeqCst);
    c.ok(
        Method::POST,
        &format!("/api/projects/{}/buckets", c.project),
        Some(json!({ "name": "files", "environmentId": c.staging, "bucket": "files-staging" })),
    )
    .await;
    assert_eq!(c.cf.token_ids().len(), 1);
    c.ok(
        Method::DELETE,
        &format!("/api/environments/{}", c.staging),
        None,
    )
    .await;
    assert!(c.cf.token_ids().is_empty());
    assert_eq!(c.cf.buckets(), ["files-staging"]);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn self_hosted_buckets_are_a_template(pool: PgPool) {
    let c = Ctx::new(pool).await;
    let created = c
        .ok(
            Method::POST,
            &format!("/api/projects/{}/services", c.project),
            Some(json!({ "name": "files", "template": "bucket" })),
        )
        .await;
    let inst = created["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["environmentId"] == c.prod.to_string())
        .unwrap();
    assert_eq!(inst["template"], "bucket");
    // The bucket is the volume, so it exists from the first start.
    assert_eq!(inst["volume"]["mountPath"], "/data/bucket");
    let image = inst["config"]["source"]["image"].as_str().unwrap();
    assert!(
        image.starts_with("rclone/rclone:") && image.contains("@sha256:"),
        "{image}"
    );
    let instance: Id = inst["id"].as_str().unwrap().parse().unwrap();

    let env = c.resolved(instance, c.prod).await;
    assert_eq!(env["S3_BUCKET"], "bucket");
    assert_eq!(env["BUCKET_NAME"], "bucket");
    assert_eq!(env["S3_ENDPOINT"], "http://files.modula.internal:9000");
    let (id, secret) = (&env["AWS_ACCESS_KEY_ID"], &env["AWS_SECRET_ACCESS_KEY"]);
    assert_eq!(id.len(), 32);
    assert_ne!(id, secret);
    // rclone reads its key pair as one CSV field.
    assert_eq!(env["RCLONE_AUTH_KEY"], format!("\"{id},{secret}\""));

    // Each environment has its own credentials.
    let staging = c.instance("files", c.staging).await;
    let other = c.resolved(staging, c.staging).await;
    assert_ne!(other["AWS_SECRET_ACCESS_KEY"], *secret);

    // Not an R2 bucket: no Cloudflare endpoints apply.
    let r = c
        .call(
            Method::POST,
            &format!("/api/service-instances/{instance}/r2"),
            Some(json!({ "bucket": "files" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = c
        .call(
            Method::POST,
            &format!("/api/projects/{}/services", c.project),
            Some(json!({ "name": "r2", "template": "r2" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn duplicated_environments_get_no_bucket_credentials(pool: PgPool) {
    let c = Ctx::new(pool).await;
    c.connect().await;
    c.cf.tokens_allowed.store(true, Ordering::SeqCst);
    c.ok(
        Method::POST,
        &format!("/api/projects/{}/buckets", c.project),
        Some(json!({ "name": "media", "environmentId": c.prod, "bucket": "media-prod" })),
    )
    .await;
    assert!(
        !c.variables(c.instance("media", c.prod).await)
            .await
            .is_empty()
    );
    let r = c
        .ok(
            Method::POST,
            &format!("/api/projects/{}/environments", c.project),
            Some(json!({ "name": "preview", "duplicateFrom": c.prod })),
        )
        .await;
    let preview: Id = r["id"].as_str().unwrap().parse().unwrap();
    // The service exists there, without a bucket or its credentials.
    let instance = c.instance("media", preview).await;
    assert!(c.variables(instance).await.is_empty());
    let buckets: i64 =
        sqlx::query_scalar("SELECT count(*) FROM r2_buckets WHERE service_instance_id = $1")
            .bind(instance)
            .fetch_one(c.app.db())
            .await
            .unwrap();
    assert_eq!(buckets, 0);
    assert_eq!(c.cf.token_ids().len(), 1);
}
