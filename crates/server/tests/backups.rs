//! Backups end to end over real gRPC + mTLS, with an in-memory runtime and an
//! in-process S3 stand-in: a manual backup is dumped on the node, lands in
//! the bucket encrypted (and decrypts with the workspace's key), restores
//! into the database container, and is deleted. Plus the control plane's own
//! backup, when `pg_dump` is installed.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use common::*;
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use modula_server::backup;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------- mock S3

/// Path-style S3: objects, multipart uploads (parts joined in part order on
/// completion, checked against the ETags it handed out). No signature checks.
/// Responses are shaped like AWS's, namespace included.
/// An upload's key, and its parts: part number → (etag, bytes).
type Upload = (String, BTreeMap<u16, (String, Vec<u8>)>);

#[derive(Default)]
struct MockS3 {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    uploads: Mutex<HashMap<String, Upload>>,
}

const S3_NS: &str = "http://s3.amazonaws.com/doc/2006-03-01/";

async fn s3_handler(
    State(s3): State<Arc<MockS3>>,
    method: Method,
    Path((bucket, key)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let key = format!("{bucket}/{key}");
    let xml = |s: String| ([(header::CONTENT_TYPE, "application/xml")], s).into_response();
    match (method, q.get("uploadId")) {
        (Method::POST, None) if q.contains_key("uploads") => {
            let id = format!("upload-{}", Id::new());
            s3.uploads
                .lock()
                .unwrap()
                .insert(id.clone(), (key.clone(), BTreeMap::new()));
            xml(format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<InitiateMultipartUploadResult xmlns=\"{S3_NS}\"><Bucket>{bucket}</Bucket><Key>{key}</Key><UploadId>{id}</UploadId></InitiateMultipartUploadResult>"
            ))
        }
        (Method::PUT, Some(upload)) => {
            let n: u16 = q["partNumber"].parse().unwrap();
            let etag = format!("\"etag-{n}-{}\"", body.len());
            let mut uploads = s3.uploads.lock().unwrap();
            let Some((_, parts)) = uploads.get_mut(upload) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            parts.insert(n, (etag.clone(), body.to_vec()));
            let mut h = HeaderMap::new();
            h.insert(header::ETAG, etag.parse().unwrap());
            (h, "").into_response()
        }
        (Method::POST, Some(upload)) => {
            let Some((k, parts)) = s3.uploads.lock().unwrap().remove(upload) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            let listed = String::from_utf8_lossy(&body).replace("&quot;", "\"");
            let mut object = Vec::new();
            for (etag, bytes) in parts.values() {
                assert!(
                    listed.contains(etag.as_str()),
                    "{etag} missing from {listed}"
                );
                object.extend_from_slice(bytes);
            }
            s3.objects.lock().unwrap().insert(k.clone(), object);
            xml(format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<CompleteMultipartUploadResult xmlns=\"{S3_NS}\"><Key>{k}</Key></CompleteMultipartUploadResult>"
            ))
        }
        (Method::DELETE, Some(upload)) => {
            s3.uploads.lock().unwrap().remove(upload);
            StatusCode::NO_CONTENT.into_response()
        }
        (Method::PUT, None) => {
            s3.objects.lock().unwrap().insert(key, body.to_vec());
            (StatusCode::OK, "").into_response()
        }
        (Method::GET, None) => match s3.objects.lock().unwrap().get(&key) {
            Some(b) => b.clone().into_response(),
            None => StatusCode::NOT_FOUND.into_response(),
        },
        (Method::DELETE, None) => {
            s3.objects.lock().unwrap().remove(&key);
            StatusCode::NO_CONTENT.into_response()
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn mock_s3() -> (String, Arc<MockS3>) {
    let s3 = Arc::new(MockS3::default());
    let app = axum::Router::new()
        .route("/{bucket}/{*key}", axum::routing::any(s3_handler))
        .with_state(s3.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // The SSRF guard refuses loopback endpoints unless told otherwise.
    backup::s3::allow_local_endpoints(true);
    modula_node::backup::allow_loopback_endpoints(true);
    (format!("http://{addr}"), s3)
}

// ---------------------------------------------------------------- cluster

struct Cluster {
    app: TestApp,
    admin: String,
    stop: CancellationToken,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn cluster(app: TestApp) -> Cluster {
    let admin = app.setup_admin().await;
    let stop = CancellationToken::new();
    tokio::spawn(modula_server::jobs::run_worker(
        app.state.clone(),
        backup::register(modula_server::jobs::Registry::default()),
        "test-worker".into(),
        stop.clone(),
    ));
    Cluster { app, admin, stop }
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
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Resp {
        self.app.call(method, path, Some(&self.admin), body).await
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let r = self.call(method.clone(), path, body).await;
        assert_eq!(r.status, StatusCode::OK, "{method} {path}: {:?}", r.json);
        r.json
    }

    /// A joined node on a fake runtime.
    async fn start_node(&self) -> Arc<FakeRuntime> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(modula_server::nodes::serve(
            self.app.state.clone(),
            listener,
            self.stop.clone(),
        ));
        let t = self.ok(Method::POST, "/api/nodes/join-tokens", None).await;
        let dir = std::env::temp_dir().join(format!("modula-node-test-{}", Id::new()));
        join(JoinArgs {
            server: &format!("https://localhost:{port}"),
            token: t["token"].as_str().unwrap(),
            fingerprint: t["fingerprint"].as_str().unwrap(),
            hostname: "db-1".into(),
            dir: &dir,
        })
        .await
        .expect("join");
        let rt = Arc::new(FakeRuntime::new());
        let caps = modula_node::reconcile::Capabilities {
            edge: None,
            build: None,
            db: None,
            upgrade: None,
            mesh: None,
        };
        tokio::spawn(modula_node::session::run(
            dir,
            rt.clone(),
            caps,
            self.stop.child_token(),
        ));
        wait_for("node online", || async {
            let nodes = self.ok(Method::GET, "/api/nodes", None).await;
            nodes
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["connected"] == true)
        })
        .await;
        rt
    }

    /// A running postgres service (an image service marked as the template).
    async fn database(&self, ws: Id) -> Id {
        let p = self
            .ok(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(json!({"name": "shop"})),
            )
            .await;
        let s = self
            .ok(
                Method::POST,
                &format!("/api/projects/{}/services", p["id"].as_str().unwrap()),
                Some(json!({ "name": "db", "config": { "source": { "type": "image", "image": "postgres:17-alpine" }, "port": 5432 } })),
            )
            .await;
        sqlx::query("UPDATE services SET template = 'postgres' WHERE id = $1")
            .bind(s["id"].as_str().unwrap())
            .execute(self.app.db())
            .await
            .unwrap();
        let instance: Id = s["instances"][0]["id"].as_str().unwrap().parse().unwrap();
        let d = self
            .ok(
                Method::POST,
                &format!("/api/service-instances/{instance}/deployments"),
                None,
            )
            .await;
        let d = d["id"].as_str().unwrap().to_owned();
        wait_for("database running", || async {
            let r = self
                .ok(Method::GET, &format!("/api/deployments/{d}"), None)
                .await;
            r["status"] == "active"
        })
        .await;
        instance
    }

    async fn target(&self, ws: Id, endpoint: &str) -> String {
        let r = self
            .ok(
                Method::POST,
                &format!("/api/workspaces/{ws}/backup-targets"),
                Some(json!({
                    "name": "bucket", "provider": "minio", "endpoint": endpoint,
                    "bucket": "backups", "prefix": "/team/", "accessKeyId": "AKID",
                    "secretAccessKey": "very-secret-key",
                })),
            )
            .await;
        assert!(!r.to_string().contains("very-secret-key"));
        assert_eq!(r["prefix"], "team/");
        r["id"].as_str().unwrap().to_owned()
    }
}

fn decrypt(ciphertext: &[u8], identity: &str) -> Vec<u8> {
    let identity = modula_backup::keys::parse_identity(identity).unwrap();
    let mut out = Vec::new();
    modula_backup::stream::decrypt_reader(ciphertext, &identity)
        .unwrap()
        .read_to_end(&mut out)
        .unwrap();
    out
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn backup_restore_and_delete(pool: PgPool) {
    let (endpoint, s3) = mock_s3().await;
    let c = cluster(TestApp::new(pool)).await;
    let rt = c.start_node().await;
    let dump: Vec<u8> = b"PGDMP fake archive "
        .iter()
        .copied()
        .cycle()
        .take(300_000)
        .collect();
    *rt.exec_stdout.lock().unwrap() = dump.clone();

    let ws = c.app.create_workspace(&c.admin, "team").await;
    let instance = c.database(ws).await;
    let target = c.target(ws, &endpoint).await;
    assert!(s3.objects.lock().unwrap().is_empty(), "the probe cleans up");

    // Nowhere to back up to until a schedule picks a target.
    let backups = format!("/api/service-instances/{instance}/backups");
    let r = c.call(Method::POST, &backups, None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let r = c
        .ok(
            Method::PUT,
            &format!("/api/service-instances/{instance}/backup-schedule"),
            Some(json!({ "enabled": true, "targetId": target, "hourUtc": 3 })),
        )
        .await;
    assert_eq!(r["schedule"]["retention"]["weeklyDays"], 27);

    let b = c.ok(Method::POST, &backups, None).await;
    assert_eq!(b["status"], "running");
    assert_eq!(b["kind"], "manual");
    let id = b["id"].as_str().unwrap().to_owned();
    wait_for("backup completed", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["backups"][0]["status"] == "completed"
    })
    .await;

    // The dump ran in the database's container, as its superuser.
    let exec = rt.execs.lock().unwrap()[0].clone();
    assert_eq!(exec.cmd[0], "pg_dump");
    assert!(exec.cmd.contains(&"--username=postgres".to_owned()));

    // In the bucket, encrypted; the workspace's identity decrypts it.
    let (key, object) = s3.objects.lock().unwrap().pop_first().unwrap();
    assert!(
        key.starts_with(&format!("backups/team/{ws}/{instance}/")) && key.ends_with(".dump.age"),
        "{key}"
    );
    assert!(
        !object.windows(5).any(|w| w == b"PGDMP"),
        "stored in the clear"
    );
    let identity = backup::identity(&c.app.state, ws).await.unwrap();
    assert_eq!(decrypt(&object, &identity), dump);
    let listed = c.ok(Method::GET, &backups, None).await;
    assert_eq!(listed["backups"][0]["sizeBytes"], object.len());
    assert_eq!(listed["backups"][0]["targetName"], "bucket");
    s3.objects.lock().unwrap().insert(key.clone(), object);

    // Restore pipes the decrypted dump into pg_restore.
    let r = c
        .ok(Method::POST, &format!("/api/backups/{id}/restore"), None)
        .await;
    // The fake runtime can finish before the response is read back.
    assert!(
        r["status"] == "running" || r["status"] == "completed",
        "{r}"
    );
    wait_for("restore completed", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["restores"][0]["status"] == "completed"
    })
    .await;
    let exec = rt.execs.lock().unwrap().last().unwrap().clone();
    assert_eq!(exec.cmd[0], "pg_restore");
    assert!(exec.cmd.contains(&"--single-transaction".to_owned()));
    assert_eq!(exec.stdin.as_deref(), Some(&dump[..]));

    // Deleting removes the object.
    let r = c
        .ok(Method::DELETE, &format!("/api/backups/{id}"), None)
        .await;
    assert_eq!(r["status"], "deleted");
    assert!(s3.objects.lock().unwrap().is_empty());

    // The target is in use by the schedule, so it stays.
    let r = c
        .call(
            Method::DELETE,
            &format!("/api/backup-targets/{target}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'backup%' ORDER BY id")
            .fetch_all(c.app.db())
            .await
            .unwrap();
    assert_eq!(
        actions,
        [
            "backup_target.create",
            "backup.schedule.update",
            "backup.create",
            "backup.restore",
            "backup.delete"
        ]
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_node_cannot_report_on_another_nodes_backup(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let chain = app.seed_chain(ws).await;
    let running = Id::new();
    sqlx::query(
        "INSERT INTO backups (id, service_instance_id, target_id, node_id, kind, engine, object_key, upload_id)
         VALUES ($1, $2, $3, $4, 'manual', 'postgres', 'k', 'u')",
    )
    .bind(running)
    .bind(chain.instance)
    .bind(chain.backup_target)
    .bind(chain.node)
    .execute(app.db())
    .await
    .unwrap();
    let forged = modula_proto::node::v1::BackupResult {
        id: running.to_string(),
        ok: true,
        size_bytes: 1,
        ..Default::default()
    };
    backup::on_result(&app.state, Id::new(), forged)
        .await
        .unwrap();
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'backup.finish'")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(queued, 0);
}

/// Needs `pg_dump` on PATH (skipped otherwise).
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_control_plane_backs_itself_up(pool: PgPool) {
    if std::process::Command::new("pg_dump")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipped: pg_dump is not installed");
        return;
    }
    // pg_dump connects to this test's own database.
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut url: url::Url = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
    url.set_path(&db);
    let mut cfg = config();
    cfg.database_url = url.to_string();

    let (endpoint, s3) = mock_s3().await;
    let c = cluster(TestApp::with_config(pool, cfg)).await;
    let ws = c.app.create_workspace(&c.admin, "ops").await;
    let target = c.target(ws, &endpoint).await;
    let r = c
        .ok(
            Method::PUT,
            "/api/instance/self-backup",
            Some(json!({ "enabled": true, "targetId": target, "hourUtc": 4 })),
        )
        .await;
    assert_eq!(r["enabled"], true);

    let id = backup::run::start(
        &c.app.state,
        None,
        target.parse().unwrap(),
        backup::Kind::Manual,
        None,
    )
    .await
    .unwrap();
    wait_for("self-backup completed", || async {
        let r = c.ok(Method::GET, "/api/instance/self-backup", None).await;
        r["backups"][0]["status"] != "running"
    })
    .await;
    let r = c.ok(Method::GET, "/api/instance/self-backup", None).await;
    assert_eq!(r["backups"][0]["status"], "completed", "{r}");
    assert_eq!(r["backups"][0]["id"], id.to_string());

    // Not visible through a workspace's backup routes.
    let r = c
        .call(Method::DELETE, &format!("/api/backups/{id}"), None)
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    let (key, object) = s3.objects.lock().unwrap().pop_first().unwrap();
    assert!(
        key.starts_with("backups/team/modula-control-plane/"),
        "{key}"
    );
    let identity = backup::control_plane::identity(&c.app.state.keys).unwrap();
    let mut plain = Vec::new();
    modula_backup::stream::decrypt_reader(&object[..], &identity)
        .unwrap()
        .read_to_end(&mut plain)
        .unwrap();
    assert!(plain.starts_with(b"PGDMP"), "a pg_dump custom archive");

    // The documented recovery: restore into an empty database and find the
    // data again (the restore CLI does this with the master key).
    // Database names cannot be bound; this one is generated here.
    let scratch = format!("modula_restore_{}", Id::new());
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE \"{scratch}\""
    )))
    .execute(c.app.db())
    .await
    .unwrap();
    let mut restored_url = url.clone();
    restored_url.set_path(&scratch);
    backup::control_plane::restore(&object[..], &identity, restored_url.as_str()).unwrap();
    let restored = PgPool::connect(restored_url.as_str()).await.unwrap();
    let names: Vec<String> = sqlx::query_scalar("SELECT name FROM workspaces ORDER BY name")
        .fetch_all(&restored)
        .await
        .unwrap();
    assert!(names.contains(&"ops".to_owned()), "{names:?}");
    restored.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE \"{scratch}\"")))
        .execute(c.app.db())
        .await
        .unwrap();
}

// ---------------------------------------------------------------- real storage

/// A real S3-compatible store, from `MODULA_TEST_S3_ENDPOINT` (e.g. a local
/// MinIO), `MODULA_TEST_S3_ACCESS_KEY`, `MODULA_TEST_S3_SECRET_KEY` and
/// `MODULA_TEST_S3_BUCKET` (which must exist). For example:
///
/// ```sh
/// docker run -d --rm -p 127.0.0.1:9100:9000 -e MINIO_ROOT_USER=modulatest \
///   -e MINIO_ROOT_PASSWORD=modulatest-secret minio/minio server /data
/// curl -X PUT --aws-sigv4 aws:amz:us-east-1:s3 \
///   --user modulatest:modulatest-secret http://127.0.0.1:9100/backups
/// ```
fn real_target() -> backup::s3::Target {
    let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("set {name}"));
    backup::s3::allow_local_endpoints(true);
    modula_node::backup::allow_loopback_endpoints(true);
    backup::s3::Target {
        provider: backup::s3::Provider::Minio,
        endpoint: var("MODULA_TEST_S3_ENDPOINT").parse().unwrap(),
        region: "us-east-1".into(),
        bucket: var("MODULA_TEST_S3_BUCKET"),
        prefix: format!("modula-test-{}/", Id::new()),
        access_key_id: var("MODULA_TEST_S3_ACCESS_KEY"),
        secret: zeroize::Zeroizing::new(var("MODULA_TEST_S3_SECRET_KEY")),
    }
}

/// Signing, multipart uploads of several fixed-size parts, download and
/// decryption against real storage.
#[tokio::test]
#[ignore = "needs an S3-compatible store (MODULA_TEST_S3_*)"]
async fn real_storage_round_trip() {
    let target = real_target();
    target.probe().await.expect("probe");

    // 12 MiB of noise in 5 MiB parts: three parts, the last one short.
    let mut x = 0x2545_f491_4f6c_dd1d_u64;
    let plaintext: Vec<u8> = (0..12 * 1024 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let (recipient, identity) = modula_backup::keys::generate();
    let recipient = modula_backup::keys::parse_recipient(&recipient).unwrap();
    let identity = modula_backup::keys::parse_identity(&identity).unwrap();

    let key = target.key("db.dump.age");
    let upload_id = target.create_multipart(&key).await.expect("create");
    let urls = target.presign_parts(&key, &upload_id, 10).unwrap();
    let chunks = plaintext
        .chunks(100_000)
        .map(|c| Ok::<_, anyhow::Error>(Bytes::copy_from_slice(c)))
        .collect::<Vec<_>>();
    let uploaded = modula_backup::stream::encrypt_and_upload(
        backup::s3::http().unwrap(),
        futures::stream::iter(chunks),
        &recipient,
        &urls,
        modula_backup::stream::MIN_PART_SIZE,
    )
    .await
    .expect("upload");
    assert_eq!(uploaded.etags.len(), 3);
    target
        .complete_multipart(&key, &upload_id, &uploaded.etags)
        .await
        .expect("complete");

    let url = target.presign_get(&key).unwrap();
    let mut stream =
        modula_backup::stream::download_and_decrypt(backup::s3::http().unwrap(), &url, &identity)
            .await
            .expect("download");
    let mut out = Vec::new();
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        out.extend_from_slice(&chunk.unwrap());
    }
    assert!(out == plaintext, "the round trip changed the data");

    // An aborted upload leaves nothing behind; deleting twice is fine.
    let aborted = target.create_multipart(&target.key("x")).await.unwrap();
    target
        .abort_multipart(&target.key("x"), &aborted)
        .await
        .unwrap();
    target.delete(&key).await.unwrap();
    target.delete(&key).await.unwrap();
    let gone = backup::s3::http().unwrap().get(&url).send().await.unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

/// `docker …`, panicking with its stderr on failure.
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

fn psql(container: &str, sql: &str) -> String {
    docker(&[
        "exec",
        container,
        "psql",
        "-U",
        "app",
        "-d",
        "shop",
        "-At",
        "--command",
        sql,
    ])
}

/// A real postgres:17-alpine container, dumped and restored by the node's
/// backup code through the Docker runtime and real storage.
#[tokio::test]
#[ignore = "needs Docker and an S3-compatible store (MODULA_TEST_S3_*)"]
async fn real_postgres_backup_and_restore() {
    let target = real_target();
    let deployment = Id::new().to_string();
    let name = format!("modula-backup-test-{deployment}");
    docker(&[
        "run",
        "-d",
        "--rm",
        "--name",
        &name,
        "--label",
        "modula.managed=true",
        "--label",
        &format!("modula.deployment={deployment}"),
        "-e",
        "POSTGRES_USER=app",
        "-e",
        "POSTGRES_PASSWORD=pw-not-needed-on-the-socket",
        "-e",
        "POSTGRES_DB=shop",
        "postgres:17-alpine",
    ]);
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
        let ok = std::process::Command::new("docker")
            .args([
                "exec",
                &name,
                "psql",
                "-U",
                "app",
                "-d",
                "shop",
                "-h",
                "127.0.0.1",
                "--command",
                "SELECT 1",
            ])
            .env("PGPASSWORD", "pw-not-needed-on-the-socket")
            .output()
            .is_ok_and(|o| o.status.success());
        if ok {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ready, "postgres did not start");
    psql(
        &name,
        "CREATE TABLE orders (id int PRIMARY KEY, item text);
         INSERT INTO orders SELECT g, 'item ' || g FROM generate_series(1, 5000) g;",
    );

    let runtime: Arc<dyn modula_node::runtime::Runtime> =
        Arc::new(modula_node::runtime::docker::DockerRuntime::connect().unwrap());
    let (recipient, identity) = modula_backup::keys::generate();
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    let result = |m: modula_proto::node::v1::NodeMessage| match m.msg {
        Some(modula_proto::node::v1::node_message::Msg::BackupResult(r)) => r,
        other => panic!("unexpected {other:?}"),
    };

    let key = target.key("orders.dump.age");
    let upload_id = target.create_multipart(&key).await.unwrap();
    modula_node::backup::backup(
        runtime.clone(),
        modula_proto::node::v1::BackupRequest {
            backup_id: "b1".into(),
            deployment_id: deployment.clone(),
            engine: modula_proto::node::v1::DatabaseEngine::EnginePostgres as i32,
            age_recipient: recipient,
            part_urls: target.presign_parts(&key, &upload_id, 5).unwrap(),
            part_size: modula_backup::stream::MIN_PART_SIZE as u64,
        },
        tx.clone(),
    )
    .await;
    let r = result(rx.recv().await.unwrap());
    assert!(r.ok, "backup failed: {}", r.error);
    target
        .complete_multipart(&key, &upload_id, &r.etags)
        .await
        .unwrap();

    // Lose data, then restore it.
    psql(
        &name,
        "DELETE FROM orders WHERE id > 10; CREATE TABLE junk (x int);",
    );
    modula_node::backup::restore(
        runtime.clone(),
        modula_proto::node::v1::RestoreRequest {
            restore_id: "r1".into(),
            deployment_id: deployment.clone(),
            engine: modula_proto::node::v1::DatabaseEngine::EnginePostgres as i32,
            object_url: target.presign_get(&key).unwrap(),
            age_identity: (*identity).clone(),
        },
        tx.clone(),
    )
    .await;
    let r = result(rx.recv().await.unwrap());
    assert!(r.ok, "restore failed: {}", r.error);
    assert_eq!(
        psql(&name, "SELECT count(*), max(item) FROM orders"),
        "5000|item 999"
    );

    // Another workspace's key cannot decrypt it; nothing is touched.
    modula_node::backup::restore(
        runtime.clone(),
        modula_proto::node::v1::RestoreRequest {
            restore_id: "r2".into(),
            deployment_id: deployment.clone(),
            engine: modula_proto::node::v1::DatabaseEngine::EnginePostgres as i32,
            object_url: target.presign_get(&key).unwrap(),
            age_identity: (*modula_backup::keys::generate().1).clone(),
        },
        tx.clone(),
    )
    .await;
    let r = result(rx.recv().await.unwrap());
    assert!(!r.ok);
    assert_eq!(psql(&name, "SELECT count(*) FROM orders"), "5000");

    // A dump that fails reports the tool's error, not a silent success.
    let upload_id = target.create_multipart(&key).await.unwrap();
    modula_node::backup::backup(
        runtime.clone(),
        modula_proto::node::v1::BackupRequest {
            backup_id: "b2".into(),
            deployment_id: deployment.clone(),
            engine: modula_proto::node::v1::DatabaseEngine::EngineMongo as i32,
            age_recipient: modula_backup::keys::generate().0,
            part_urls: target.presign_parts(&key, &upload_id, 5).unwrap(),
            part_size: modula_backup::stream::MIN_PART_SIZE as u64,
        },
        tx,
    )
    .await;
    let r = result(rx.recv().await.unwrap());
    assert!(!r.ok);
    assert!(
        r.error.contains("mongodump is not available"),
        "{}",
        r.error
    );
    target.abort_multipart(&key, &upload_id).await.unwrap();
    target.delete(&key).await.unwrap();
}
