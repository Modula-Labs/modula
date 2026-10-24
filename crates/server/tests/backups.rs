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
        self.start_node_with(None).await
    }

    /// A joined node on a fake runtime, with a fake Database executor.
    async fn start_node_with(
        &self,
        db: Option<Arc<dyn modula_node::db::Executor>>,
    ) -> Arc<FakeRuntime> {
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
            db,
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

impl Cluster {
    /// A running self-hosted bucket service (template `bucket`) in its own
    /// project, listening on `port` (the mock S3's, which the fake runtime
    /// reports as every container's address).
    async fn bucket_service(&self, ws: Id, port: u16) -> Id {
        let p = self
            .ok(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(json!({"name": "files"})),
            )
            .await;
        let s = self
            .ok(
                Method::POST,
                &format!("/api/projects/{}/services", p["id"].as_str().unwrap()),
                Some(json!({ "name": "files", "template": "bucket" })),
            )
            .await;
        let instance: Id = s["instances"][0]["id"].as_str().unwrap().parse().unwrap();
        sqlx::query(
            "UPDATE service_instances SET config = jsonb_set(config, '{port}', to_jsonb($2::int))
             WHERE id = $1",
        )
        .bind(instance)
        .bind(i32::from(port))
        .execute(self.app.db())
        .await
        .unwrap();
        let d = self
            .ok(
                Method::POST,
                &format!("/api/service-instances/{instance}/deployments"),
                None,
            )
            .await;
        let d = d["id"].as_str().unwrap().to_owned();
        wait_for("bucket running", || async {
            let r = self
                .ok(Method::GET, &format!("/api/deployments/{d}"), None)
                .await;
            r["status"] == "active"
        })
        .await;
        instance
    }
}

/// A self-hosted bucket (only the private network reaches it: the node does
/// the requests) and the database server's own disk, end to end.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn backups_to_a_bucket_service_and_to_the_servers_disk(pool: PgPool) {
    let (endpoint, s3) = mock_s3().await;
    let port: u16 = endpoint.rsplit(':').next().unwrap().parse().unwrap();
    let disk = std::env::temp_dir().join(format!("modula-disk-{}", Id::new()));
    modula_node::backup::set_dir(disk.clone());
    let c = cluster(TestApp::new(pool)).await;
    let rt = c.start_node().await;
    *rt.address_override.lock().unwrap() = Some("127.0.0.1".parse().unwrap());
    let dump: Vec<u8> = b"PGDMP to a bucket "
        .iter()
        .copied()
        .cycle()
        .take(100_000)
        .collect();
    *rt.exec_stdout.lock().unwrap() = dump.clone();
    let ws = c.app.create_workspace(&c.admin, "team").await;
    let instance = c.database(ws).await;
    let bucket = c.bucket_service(ws, port).await;

    // The Storage choices: the bucket, and the database server's disk.
    let dest = format!("/api/service-instances/{instance}/backup-destinations");
    let d = c.ok(Method::GET, &dest, None).await;
    assert_eq!(d["buckets"][0]["instanceId"], bucket.to_string());
    assert_eq!(d["buckets"][0]["template"], "bucket");
    assert!(d["buckets"][0]["targetId"].is_null());
    assert_eq!(d["servers"][0]["name"], "db-1");

    let schedule = format!("/api/service-instances/{instance}/backup-schedule");
    let r = c
        .call(
            Method::PUT,
            &schedule,
            Some(json!({ "enabled": true, "hourUtc": 3, "localDisk": true,
                         "bucketInstanceId": bucket })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "one destination");
    let r = c
        .ok(
            Method::PUT,
            &schedule,
            Some(json!({ "enabled": true, "hourUtc": 3, "bucketInstanceId": bucket })),
        )
        .await;
    assert_eq!(r["schedule"]["targetKind"], "bucket");
    assert_eq!(r["schedule"]["targetName"], "files (production)");
    // Choosing it again reuses its target; Settings lists it with its service.
    let again = c
        .ok(
            Method::PUT,
            &schedule,
            Some(json!({ "enabled": true, "hourUtc": 4, "bucketInstanceId": bucket })),
        )
        .await;
    assert_eq!(again["schedule"]["targetId"], r["schedule"]["targetId"]);
    let targets = c
        .ok(
            Method::GET,
            &format!("/api/workspaces/{ws}/backup-targets"),
            None,
        )
        .await;
    assert_eq!(targets.as_array().unwrap().len(), 1);
    assert_eq!(targets[0]["kind"], "bucket");
    assert_eq!(targets[0]["serviceName"], "files");
    assert_eq!(targets[0]["projectName"], "files");

    let backups = format!("/api/service-instances/{instance}/backups");
    let b = c.ok(Method::POST, &backups, None).await;
    let id = b["id"].as_str().unwrap().to_owned();
    wait_for("backup to the bucket", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["backups"][0]["status"] == "completed"
    })
    .await;
    // The node did the multipart upload itself, under the bucket's prefix.
    let (key, object) = s3
        .objects
        .lock()
        .unwrap()
        .first_key_value()
        .map(|(k, v)| (k.clone(), v.clone()))
        .unwrap();
    assert!(
        key.starts_with(&format!("bucket/modula-backups/{ws}/{instance}/")),
        "{key}"
    );
    assert!(s3.uploads.lock().unwrap().is_empty(), "completed");
    let identity = backup::identity(&c.app.state, ws).await.unwrap();
    assert_eq!(decrypt(&object, &identity), dump);

    c.ok(Method::POST, &format!("/api/backups/{id}/restore"), None)
        .await;
    wait_for("restore from the bucket", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["restores"][0]["status"] == "completed"
    })
    .await;
    let exec = rt.execs.lock().unwrap().last().unwrap().clone();
    assert_eq!(exec.stdin.as_deref(), Some(&dump[..]));

    // Deleting goes through the bucket's server, which reports back.
    let r = c
        .ok(Method::DELETE, &format!("/api/backups/{id}"), None)
        .await;
    assert!(r["status"] == "deleting" || r["status"] == "deleted", "{r}");
    wait_for("deleted from the bucket", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["backups"][0]["status"] == "deleted"
    })
    .await;
    assert!(s3.objects.lock().unwrap().is_empty());

    // The server's disk.
    let r = c
        .ok(
            Method::PUT,
            &schedule,
            Some(json!({ "enabled": true, "hourUtc": 3, "localDisk": true })),
        )
        .await;
    assert_eq!(r["schedule"]["targetKind"], "local");
    assert_eq!(r["schedule"]["targetName"], "Disk of db-1");
    let b = c.ok(Method::POST, &backups, None).await;
    let id = b["id"].as_str().unwrap().to_owned();
    wait_for("backup to disk", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["backups"][0]["status"] == "completed"
    })
    .await;
    let dir = disk.join(ws.to_string()).join(instance.to_string());
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].to_string_lossy().ends_with(".dump.age"));
    assert_eq!(decrypt(&std::fs::read(&files[0]).unwrap(), &identity), dump);
    let listed = c.ok(Method::GET, &backups, None).await;
    assert_eq!(listed["backups"][0]["targetKind"], "local");

    c.ok(Method::POST, &format!("/api/backups/{id}/restore"), None)
        .await;
    wait_for("restore from disk", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["restores"][0]["status"] == "completed"
    })
    .await;
    c.ok(Method::DELETE, &format!("/api/backups/{id}"), None)
        .await;
    wait_for("deleted from disk", || async {
        let r = c.ok(Method::GET, &backups, None).await;
        r["backups"][0]["status"] == "deleted"
    })
    .await;
    assert!(!files[0].exists());

    // The control plane can't back itself up to either.
    let local: Id = sqlx::query_scalar("SELECT id FROM backup_targets WHERE kind = 'local'")
        .fetch_one(c.app.db())
        .await
        .unwrap();
    let r = c
        .call(
            Method::PUT,
            "/api/instance/self-backup",
            Some(json!({ "enabled": true, "targetId": local, "hourUtc": 3 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let _ = std::fs::remove_dir_all(disk);
}

/// An R2 bucket service's target is its instance's variables, read when used.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn r2_bucket_variables_resolve_to_a_target(pool: PgPool) {
    let c = cluster(TestApp::new(pool)).await;
    let ws = c.app.create_workspace(&c.admin, "team").await;
    let p = c
        .ok(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(json!({"name": "media"})),
        )
        .await;
    let s = c
        .ok(
            Method::POST,
            &format!("/api/projects/{}/services", p["id"].as_str().unwrap()),
            Some(json!({ "name": "media", "config": { "source": { "type": "image", "image": "busybox" } } })),
        )
        .await;
    let bucket: Id = s["instances"][0]["id"].as_str().unwrap().parse().unwrap();
    sqlx::query("UPDATE services SET template = 'r2' WHERE id = $1")
        .bind(s["id"].as_str().unwrap())
        .execute(c.app.db())
        .await
        .unwrap();
    c.ok(
        Method::PATCH,
        &format!("/api/service-instances/{bucket}/variables"),
        Some(json!({
            "set": {
                "S3_BUCKET": "media-prod",
                "BUCKET_NAME": "${{S3_BUCKET}}",
                "S3_ENDPOINT": "https://0123456789abcdef.r2.cloudflarestorage.com",
                "S3_REGION": "auto",
                "AWS_ACCESS_KEY_ID": "AKR2",
                "AWS_SECRET_ACCESS_KEY": "r2-secret",
            },
            "sealed": ["AWS_SECRET_ACCESS_KEY"],
        })),
    )
    .await;
    let target = Id::new();
    sqlx::query(
        "INSERT INTO backup_targets (id, workspace_id, name, kind, provider, prefix, bucket_instance_id)
         VALUES ($1, $2, 'media', 'bucket', 'r2', 'modula-backups/', $3)",
    )
    .bind(target)
    .bind(ws)
    .bind(bucket)
    .execute(c.app.db())
    .await
    .unwrap();
    let (owner, store) = backup::load_target(&c.app.state, target).await.unwrap();
    assert_eq!(owner, ws);
    let backup::Store::Remote(t) = store else {
        panic!("R2 is reached by the control plane");
    };
    assert_eq!(t.provider, backup::s3::Provider::R2);
    assert_eq!(
        t.endpoint.as_str(),
        "https://0123456789abcdef.r2.cloudflarestorage.com/"
    );
    assert_eq!(
        (t.bucket.as_str(), t.region.as_str()),
        ("media-prod", "auto")
    );
    assert_eq!(t.access_key_id, "AKR2");
    assert_eq!(t.secret.as_str(), "r2-secret");
    assert_eq!(t.key("x"), "modula-backups/x");

    // An edited endpoint is checked like any user input.
    c.ok(
        Method::PATCH,
        &format!("/api/service-instances/{bucket}/variables"),
        Some(json!({ "set": { "S3_ENDPOINT": "http://169.254.169.254" } })),
    )
    .await;
    assert!(backup::load_target(&c.app.state, target).await.is_err());
}

impl Cluster {
    async fn active_deployment(&self, instance: Id, not: Option<&str>) -> String {
        for _ in 0..200 {
            let id: Option<Id> = sqlx::query_scalar(
                "SELECT id FROM deployments WHERE service_instance_id = $1 AND status = 'active'
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(instance)
            .fetch_optional(self.app.db())
            .await
            .unwrap();
            if let Some(id) = id.map(|i| i.to_string())
                && Some(id.as_str()) != not
            {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for a new active deployment");
    }
}

fn rfc3339(t: time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

/// PITR with a fake node and runtime: turning it on redeploys with WAL-G's
/// settings, base backups run on the node and are pruned to the window, and
/// restores (into a new service, or over the same one) start deployments
/// that replay to the chosen time.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn point_in_time_recovery_end_to_end(pool: PgPool) {
    let (endpoint, _s3) = mock_s3().await;
    let c = cluster(TestApp::new(pool)).await;
    let db = Arc::new(modula_node::db::fake::FakeDb::default());
    let rt = c.start_node_with(Some(db.clone())).await;
    let ws = c.app.create_workspace(&c.admin, "team").await;
    let instance = c.database(ws).await;
    c.ok(
        Method::PATCH,
        &format!("/api/service-instances/{instance}/variables"),
        Some(json!({ "set": { "POSTGRES_PASSWORD": "source-password" } })),
    )
    .await;
    // As the template makes it.
    sqlx::query(
        "INSERT INTO volumes (id, service_instance_id, mount_path)
         VALUES ($1, $2, '/var/lib/postgresql/data')",
    )
    .bind(Id::new())
    .bind(instance)
    .execute(c.app.db())
    .await
    .unwrap();
    let target = c.target(ws, &endpoint).await;
    let pitr = format!("/api/service-instances/{instance}/pitr");
    let first = c.active_deployment(instance, None).await;

    // On: the database is redeployed with WAL-G's settings and mount.
    let r = c
        .ok(
            Method::PUT,
            &pitr,
            Some(json!({ "enabled": true, "targetId": target, "windowDays": 7 })),
        )
        .await;
    assert_eq!(r["config"]["enabled"], true);
    assert_eq!(r["config"]["windowDays"], 7);
    let second = c.active_deployment(instance, Some(&first)).await;
    let env = rt.env_of(&second);
    assert_eq!(
        env["WALG_S3_PREFIX"],
        format!("s3://backups/team/{ws}/{instance}/wal-g")
    );
    assert_eq!(env["AWS_ENDPOINT"], endpoint);
    assert_eq!(env["AWS_S3_FORCE_PATH_STYLE"], "true");
    assert_eq!(env["MODULA_PITR"], "1");
    assert_eq!(env["WALG_LIBSODIUM_KEY"].len(), 64);
    assert!(rt.workload_of(&second).unwrap().pitr.is_some());

    // A base backup: wal-g runs in the container, as postgres.
    let finished = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap()
        - time::Duration::minutes(1);
    let listed = json!([{
        "backup_name": "base_000000010000000000000003",
        "start_time": rfc3339(finished - time::Duration::seconds(5)),
        "finish_time": rfc3339(finished),
        "compressed_size": 4_000_000,
    }]);
    *rt.exec_stdout.lock().unwrap() = listed.to_string().into_bytes();
    c.ok(Method::POST, &format!("{pitr}/base-backups"), None)
        .await;
    wait_for("base backup", || async {
        let r = c.ok(Method::GET, &pitr, None).await;
        r["baseBackups"][0]["status"] == "completed"
    })
    .await;
    let execs = rt.execs.lock().unwrap().clone();
    assert_eq!(
        execs[execs.len() - 2].cmd[..4],
        ["gosu", "postgres", "wal-g", "backup-push"]
    );
    db.answer(
        "walStatus",
        Ok(json!({
            "inRecovery": false, "archiveMode": "on", "archivedCount": 9, "failedCount": 0,
            "lastArchivedAt": time::OffsetDateTime::now_utc().unix_timestamp() as f64,
            "lastFailedAt": null,
        })),
    );
    let r = c.ok(Method::GET, &pitr, None).await;
    assert_eq!(
        r["baseBackups"][0]["walgName"],
        "base_000000010000000000000003"
    );
    assert_eq!(r["baseBackups"][0]["sizeBytes"], 4_000_000);
    assert_eq!(r["window"]["from"], rfc3339(finished));
    assert!(r["archiving"]["lastArchivedAt"].is_string());
    // Base backups are not dumps: not in the dump list's restore path.
    let id = r["baseBackups"][0]["id"].as_str().unwrap().to_owned();
    let restore_dump = c
        .call(Method::POST, &format!("/api/backups/{id}/restore"), None)
        .await;
    assert_eq!(restore_dump.status, StatusCode::CONFLICT);

    // Outside the window.
    let restore = format!("{pitr}/restore");
    let r = c
        .call(
            Method::POST,
            &restore,
            Some(json!({ "targetTime": rfc3339(finished - time::Duration::hours(1)) })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);

    // Into a new service: the source's credentials, the base backup, the time.
    let at = finished + time::Duration::seconds(30);
    let r = c
        .ok(
            Method::POST,
            &restore,
            Some(json!({ "targetTime": rfc3339(at) })),
        )
        .await;
    assert_eq!(r["serviceName"], "db-restored");
    let copy: Id = r["instanceId"].as_str().unwrap().parse().unwrap();
    let dep = c.active_deployment(copy, None).await;
    let env = rt.env_of(&dep);
    assert_eq!(
        env["MODULA_PITR_RESTORE_TIME"],
        modula_server::backup::pitr::recovery_time(at)
    );
    assert_eq!(
        env["MODULA_PITR_RESTORE_BACKUP"],
        "base_000000010000000000000003"
    );
    assert_eq!(
        env["WALG_S3_PREFIX"],
        format!("s3://backups/team/{ws}/{instance}/wal-g"),
        "reads the source's WAL"
    );
    assert!(!env.contains_key("MODULA_PITR"), "never archives into it");
    assert_eq!(env["POSTGRES_PASSWORD"], "source-password");
    // Out of recovery: done.
    wait_for("restore completed", || async {
        let r = c.ok(Method::GET, &pitr, None).await;
        r["restores"][0]["status"] == "completed"
    })
    .await;

    // Over the same service: typed confirmation, then a fresh volume.
    let r = c
        .call(
            Method::POST,
            &restore,
            Some(json!({ "targetTime": rfc3339(at), "into": "same" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    let volume: Id = sqlx::query_scalar("SELECT id FROM volumes WHERE service_instance_id = $1")
        .bind(instance)
        .fetch_one(c.app.db())
        .await
        .unwrap();
    db.answer(
        "walStatus",
        Ok(json!({ "inRecovery": true, "archiveMode": "on", "lastArchivedAt": time::OffsetDateTime::now_utc().unix_timestamp() as f64 })),
    );
    let r = c
        .ok(
            Method::POST,
            &restore,
            Some(json!({ "targetTime": rfc3339(at), "into": "same", "confirm": "db" })),
        )
        .await;
    assert_eq!(r["instanceId"], instance.to_string());
    let third = c.active_deployment(instance, Some(&second)).await;
    let env = rt.env_of(&third);
    assert_eq!(env["MODULA_PITR"], "1", "archives on, a new timeline");
    assert!(env.contains_key("MODULA_PITR_RESTORE_TIME"));
    let fresh: Id = sqlx::query_scalar("SELECT id FROM volumes WHERE service_instance_id = $1")
        .bind(instance)
        .fetch_one(c.app.db())
        .await
        .unwrap();
    assert_ne!(fresh, volume);
    let queued: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM volume_removals WHERE volume_id = $1)")
            .bind(volume)
            .fetch_one(c.app.db())
            .await
            .unwrap();
    assert!(
        queued
            || rt
                .removed_volumes()
                .contains(&format!("modula-vol-{volume}"))
    );
    // Still replaying: running.
    let r = c.ok(Method::GET, &pitr, None).await;
    assert_eq!(r["restores"][0]["status"], "running");

    // Pruning: base backups older than the one covering the window go.
    for days in [10, 9] {
        sqlx::query(
            "INSERT INTO backups (id, service_instance_id, target_id, kind, engine, method,
                                  status, object_key, walg_name, finished_at)
             VALUES ($1, $2, $3, 'scheduled', 'postgres', 'walg', 'completed', 'x', $4,
                     now() - make_interval(days => $5))",
        )
        .bind(Id::new())
        .bind(instance)
        .bind(target.parse::<Id>().unwrap())
        .bind(format!("base_old_{days}"))
        .bind(days)
        .execute(c.app.db())
        .await
        .unwrap();
    }
    modula_server::backup::pitr::tick(&c.app.state)
        .await
        .unwrap();
    wait_for("old base backup pruned", || async {
        let left: Vec<String> = sqlx::query_scalar(
            "SELECT walg_name FROM backups WHERE service_instance_id = $1 AND method = 'walg'
               AND status = 'completed' ORDER BY finished_at",
        )
        .bind(instance)
        .fetch_all(c.app.db())
        .await
        .unwrap();
        left == ["base_old_9", "base_000000010000000000000003"]
    })
    .await;
    let exec = rt.execs.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        exec.cmd[3..],
        ["delete", "before", "FIND_FULL", "base_old_9", "--confirm"]
    );

    // Off: redeployed without archiving.
    c.ok(Method::PUT, &pitr, Some(json!({ "enabled": false })))
        .await;
    let fourth = c.active_deployment(instance, Some(&third)).await;
    assert!(!rt.env_of(&fourth).contains_key("MODULA_PITR"));

    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'pitr%' ORDER BY id")
            .fetch_all(c.app.db())
            .await
            .unwrap();
    assert_eq!(
        actions,
        [
            "pitr.update",
            "pitr.base_backup",
            "pitr.restore",
            "pitr.restore",
            "pitr.update"
        ]
    );
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
    target
        .probe(backup::s3::http().unwrap(), "test")
        .await
        .expect("probe");

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
    let upload_id = target
        .create_multipart(backup::s3::http().unwrap(), &key)
        .await
        .expect("create");
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
        .complete_multipart(
            backup::s3::http().unwrap(),
            &key,
            &upload_id,
            &uploaded.etags,
        )
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
    let aborted = target
        .create_multipart(backup::s3::http().unwrap(), &target.key("x"))
        .await
        .unwrap();
    target
        .abort_multipart(backup::s3::http().unwrap(), &target.key("x"), &aborted)
        .await
        .unwrap();
    target
        .delete(backup::s3::http().unwrap(), &key)
        .await
        .unwrap();
    target
        .delete(backup::s3::http().unwrap(), &key)
        .await
        .unwrap();
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
    let upload_id = target
        .create_multipart(backup::s3::http().unwrap(), &key)
        .await
        .unwrap();
    modula_node::backup::backup(
        runtime.clone(),
        modula_proto::node::v1::BackupRequest {
            backup_id: "b1".into(),
            deployment_id: deployment.clone(),
            engine: modula_proto::node::v1::DatabaseEngine::EnginePostgres as i32,
            age_recipient: recipient,
            part_urls: target.presign_parts(&key, &upload_id, 5).unwrap(),
            part_size: modula_backup::stream::MIN_PART_SIZE as u64,
            ..Default::default()
        },
        tx.clone(),
    )
    .await;
    let r = result(rx.recv().await.unwrap());
    assert!(r.ok, "backup failed: {}", r.error);
    target
        .complete_multipart(backup::s3::http().unwrap(), &key, &upload_id, &r.etags)
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
            ..Default::default()
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
            ..Default::default()
        },
        tx.clone(),
    )
    .await;
    let r = result(rx.recv().await.unwrap());
    assert!(!r.ok);
    assert_eq!(psql(&name, "SELECT count(*) FROM orders"), "5000");

    // A dump that fails reports the tool's error, not a silent success.
    let upload_id = target
        .create_multipart(backup::s3::http().unwrap(), &key)
        .await
        .unwrap();
    modula_node::backup::backup(
        runtime.clone(),
        modula_proto::node::v1::BackupRequest {
            backup_id: "b2".into(),
            deployment_id: deployment.clone(),
            engine: modula_proto::node::v1::DatabaseEngine::EngineMongo as i32,
            age_recipient: modula_backup::keys::generate().0,
            part_urls: target.presign_parts(&key, &upload_id, 5).unwrap(),
            part_size: modula_backup::stream::MIN_PART_SIZE as u64,
            ..Default::default()
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
    target
        .abort_multipart(backup::s3::http().unwrap(), &key, &upload_id)
        .await
        .unwrap();
    target
        .delete(backup::s3::http().unwrap(), &key)
        .await
        .unwrap();
}
