//! Source uploads (`modula up`) over the HTTP API: checksums, size
//! limits, the node-only download and retention. The full CLI → node build
//! path is in tests/cli.rs.

mod common;

use std::io::Write;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::*;
use http_body_util::BodyExt;
use modula_id::Id;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tower::ServiceExt;

struct Fixture {
    app: TestApp,
    admin: String,
    instance: String,
}

async fn fixture(pool: PgPool, max_bytes: u64) -> Fixture {
    let mut cfg = config();
    cfg.upload_max_bytes = max_bytes;
    let app = TestApp::with_config(pool, cfg);
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
        instance,
    }
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(data).unwrap();
    gz.finish().unwrap()
}

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

impl Fixture {
    async fn upload(
        &self,
        body: Vec<u8>,
        content_type: &str,
        sum: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder()
            .method(Method::POST)
            .uri(format!("/api/service-instances/{}/uploads", self.instance))
            .header(header::ORIGIN, ORIGIN)
            .header(header::COOKIE, format!("{COOKIE}={}", self.admin))
            .header(header::CONTENT_TYPE, content_type);
        if let Some(s) = sum {
            b = b.header("x-content-sha256", s);
        }
        let res = self
            .app
            .router
            .clone()
            .oneshot(b.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn download(&self, auth: Option<&str>) -> StatusCode {
        let mut b = Request::builder().uri("/api/build-source");
        if let Some(a) = auth {
            b = b.header(header::AUTHORIZATION, a);
        }
        let res = self
            .app
            .router
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        res.status()
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn uploads_are_checked_limited_and_never_served_to_clients(pool: PgPool) {
    let f = fixture(pool, 64 * 1024).await;
    let body = gzip(b"hello world");

    // Stored with its checksum; the file sits in the uploads directory.
    let (st, up) = f
        .upload(body.clone(), "application/gzip", Some(&sha(&body)))
        .await;
    assert_eq!(st, StatusCode::OK, "{up}");
    assert_eq!(up["sha256"], sha(&body));
    assert_eq!(up["sizeBytes"], body.len());
    let id: Id = up["id"].as_str().unwrap().parse().unwrap();
    let path = f.app.state.cfg.uploads_dir.join(format!("{id}.tar.gz"));
    assert_eq!(std::fs::read(&path).unwrap(), body);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'deployment.upload' AND target_id = $1",
    )
    .bind(id.to_string())
    .fetch_one(f.app.db())
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // A checksum mismatch, non-gzip data or the wrong type: rejected, nothing kept.
    let (st, e) = f
        .upload(body.clone(), "application/gzip", Some(&"0".repeat(64)))
        .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{e}");
    assert!(e["error"]["message"].as_str().unwrap().contains("checksum"));
    let (st, _) = f.upload(b"plain".to_vec(), "application/gzip", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = f.upload(body.clone(), "application/json", None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Over the limit: 413, whether or not the size was declared up front.
    let mut x: u32 = 7;
    let big: Vec<u8> = (0..200_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let big = gzip(&big);
    assert!(big.len() > 64 * 1024);
    let (st, e) = f.upload(big, "application/gzip", None).await;
    assert_eq!(st, StatusCode::PAYLOAD_TOO_LARGE, "{e}");
    let files = std::fs::read_dir(&f.app.state.cfg.uploads_dir)
        .unwrap()
        .count();
    assert_eq!(files, 1, "only the good upload is on disk");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM source_uploads")
        .fetch_one(f.app.db())
        .await
        .unwrap();
    assert_eq!(rows, 1);

    // Clients can't download: no token, a forged one, or a genuine token for
    // a deployment that isn't building this upload.
    assert_eq!(f.download(None).await, StatusCode::UNAUTHORIZED);
    let session = format!("Bearer {}", f.admin);
    assert_eq!(f.download(Some(&session)).await, StatusCode::UNAUTHORIZED);
    let forged = format!("Bearer {}.{id}.9999999999.{}", Id::new(), "ab".repeat(32));
    assert_eq!(f.download(Some(&forged)).await, StatusCode::UNAUTHORIZED);
    let genuine = modula_server::deploy::upload::token(
        &f.app.state.keys,
        Id::new(),
        id,
        time::OffsetDateTime::now_utc(),
    );
    assert_eq!(
        f.download(Some(&format!("Bearer {genuine}"))).await,
        StatusCode::NOT_FOUND
    );

    // An upload can't become a service's configured source.
    let r = f
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{}", f.instance),
            Some(&f.admin),
            Some(json!({ "config": { "source": { "type": "upload", "upload": id.to_string() } } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    // Deploying an unknown upload, or details without an upload, fails cleanly.
    let path_deploy = format!("/api/service-instances/{}/deployments", f.instance);
    let deploy = |body: Value| {
        f.app
            .call(Method::POST, &path_deploy, Some(&f.admin), Some(body))
    };
    let r = deploy(json!({ "source": { "upload": Id::new().to_string() } })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = deploy(json!({ "message": "hi" })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = deploy(json!({ "source": { "upload": id.to_string() }, "git": { "commit": "nope" } }))
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    // Retention: an unused upload older than a day is deleted (the row stays).
    sqlx::query("UPDATE source_uploads SET created_at = now() - interval '25 hours'")
        .execute(f.app.db())
        .await
        .unwrap();
    let purged = modula_server::deploy::upload::purge(&f.app.state)
        .await
        .unwrap();
    assert_eq!(purged, 1);
    assert!(!path.exists());
    let gone: bool =
        sqlx::query_scalar("SELECT purged_at IS NOT NULL FROM source_uploads WHERE id = $1")
            .bind(id)
            .fetch_one(f.app.db())
            .await
            .unwrap();
    assert!(gone);
    let r = deploy(json!({ "source": { "upload": id.to_string() } })).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    let _ = std::fs::remove_dir_all(&f.app.state.cfg.uploads_dir);
}
