//! Moving services with their volumes' data, end to end: real nodes (fake
//! runtimes with volumes as directories) over the gRPC stream, the copy
//! relayed through the control plane, and a local mock of the Hetzner API
//! creating the new server of a server migration (which then joins with the
//! token from its boot script).

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::*;
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use modula_server::migrate::moves::{self, Poll};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "hetzner-test-token-0123456789abcdefghijklmnopqrstuvwxyz";

/// Just enough of the Hetzner Cloud API to create a server.
#[derive(Default)]
struct Mock {
    servers: Vec<Value>,
}

type Shared = Arc<Mutex<Mock>>;

fn page(items: Value, key: &str) -> Value {
    json!({ key: items, "meta": { "pagination": { "page": 1, "per_page": 50, "next_page": null } } })
}

async fn mock_hetzner() -> (url::Url, Shared) {
    let m: Shared = Arc::default();
    let app = Router::new()
        .route(
            "/v1/locations",
            get(|| async {
                Json(page(
                    json!([
                        {"name": "fsn1", "description": "Falkenstein DC Park 1", "country": "DE", "city": "Falkenstein", "network_zone": "eu-central"},
                        {"name": "hel1", "description": "Helsinki DC Park 1", "country": "FI", "city": "Helsinki", "network_zone": "eu-central"}
                    ]),
                    "locations",
                ))
            }),
        )
        .route(
            "/v1/server_types",
            get(|| async {
                let price = |loc: &str| json!({"location": loc, "price_hourly": {"net": "0.01", "gross": "0.01"}, "price_monthly": {"net": "4.00", "gross": "4.5101000000"}});
                Json(page(
                    json!([{"name": "cx22", "description": "CX22", "cores": 2, "memory": 4.0, "disk": 40,
                            "cpu_type": "shared", "architecture": "x86", "deprecation": null,
                            "prices": [price("fsn1"), price("hel1")]}]),
                    "server_types",
                ))
            }),
        )
        .route(
            "/v1/firewalls",
            get(|| async { Json(page(json!([]), "firewalls")) }).post(|Json(mut body): Json<Value>| async move {
                body["id"] = json!(100);
                (StatusCode::CREATED, Json(json!({"firewall": body, "actions": []})))
            }),
        )
        .route(
            "/v1/firewalls/{id}/actions/set_rules",
            post(|| async { (StatusCode::CREATED, Json(json!({"actions": []}))) }),
        )
        .route(
            "/v1/servers",
            get(|| async { Json(page(json!([]), "servers")) }).post(
                |State(m): State<Shared>, Json(body): Json<Value>| async move {
                    let mut mock = m.lock().unwrap();
                    let id = 5000 + mock.servers.len() as i64;
                    let mut stored = body.clone();
                    stored["id"] = json!(id);
                    mock.servers.push(stored);
                    (
                        StatusCode::CREATED,
                        Json(json!({
                            "server": {"id": id, "name": body["name"], "status": "initializing",
                                       "public_net": {"ipv4": {"ip": "203.0.113.20"}}},
                            "action": {"id": 900, "status": "running", "progress": 0, "error": null},
                            "next_actions": []
                        })),
                    )
                },
            ),
        )
        .route(
            "/v1/servers/{id}",
            get(|Path(id): Path<i64>| async move {
                Json(json!({"server": {"id": id, "name": "x", "status": "running",
                                       "public_net": {"ipv4": {"ip": "203.0.113.20"}}}}))
            }),
        )
        .route(
            "/v1/actions/{id}",
            get(|Path(id): Path<i64>| async move {
                Json(json!({"action": {"id": id, "status": "success", "error": null}}))
            }),
        )
        .with_state(m.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1").parse().unwrap(), m)
}

struct Setup {
    app: TestApp,
    admin: String,
    grpc_port: u16,
    mock: Shared,
    stop: CancellationToken,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct Node {
    id: Id,
    rt: Arc<FakeRuntime>,
    volumes: PathBuf,
}

impl Node {
    fn volume(&self, volume: Id) -> PathBuf {
        self.volumes.join(format!("modula-vol-{volume}"))
    }
}

/// Bytes gzip can't shrink (xorshift).
fn noise(len: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

async fn wait_for<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..400 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn setup(pool: PgPool) -> Setup {
    let (url, mock) = mock_hetzner().await;
    let mut cfg = config();
    cfg.hetzner_api = url;
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    let stop = CancellationToken::new();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_port = grpc.local_addr().unwrap().port();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        grpc,
        stop.clone(),
    ));
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(
            http,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    Setup {
        app,
        admin,
        grpc_port,
        mock,
        stop,
    }
}

impl Setup {
    /// Join a node with `token` and run its session on a fake runtime whose
    /// volumes are directories.
    async fn node(&self, token: &str, name: &str) -> Node {
        let fingerprint = self.app.state.pki().await.unwrap().fingerprint();
        let dir = std::env::temp_dir().join(format!("modula-migrate-test-{}", Id::new()));
        let id = join(JoinArgs {
            server: &format!("https://localhost:{}", self.grpc_port),
            token,
            fingerprint: &fingerprint,
            hostname: name.into(),
            dir: &dir,
        })
        .await
        .expect("join");
        let volumes = dir.join("volumes");
        std::fs::create_dir_all(&volumes).unwrap();
        let rt = Arc::new(FakeRuntime::new());
        *rt.volume_root.lock().unwrap() = Some(volumes.clone());
        let caps = modula_node::reconcile::Capabilities {
            edge: None,
            build: None,
            db: None,
            upgrade: None,
            mesh: None,
            access_log: None,
            wake: None,
            functions: None,
        };
        tokio::spawn(modula_node::session::run(
            dir,
            rt.clone(),
            caps,
            self.stop.child_token(),
        ));
        let node = id.meta.node_id;
        let st = self.app.state.clone();
        wait_for("node online", || {
            let st = st.clone();
            async move { st.hub.is_connected(node) }
        })
        .await;
        Node {
            id: node,
            rt,
            volumes,
        }
    }

    async fn join_token(&self) -> String {
        let t = self
            .app
            .call(
                Method::POST,
                "/api/nodes/join-tokens",
                Some(&self.admin),
                None,
            )
            .await;
        t.json["token"].as_str().unwrap().to_owned()
    }

    async fn project(&self, ws: Id) -> String {
        let p = self
            .app
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(&self.admin),
                Some(json!({ "name": "shop" })),
            )
            .await;
        p.json["id"].as_str().unwrap().to_owned()
    }

    /// A service placed on `node`, deployed and active there; with a volume
    /// at /data (and a TCP proxy) when `volume`.
    async fn service(&self, project: &str, name: &str, node: Id, volume: bool) -> (Id, Option<Id>) {
        let s = self
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{project}/services"),
                Some(&self.admin),
                Some(json!({
                    "name": name,
                    "config": { "source": { "type": "image", "image": format!("{name}:1") }, "port": 8080 },
                })),
            )
            .await;
        assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
        let instance: Id = s.json["instances"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        sqlx::query("UPDATE service_instances SET node_id = $2 WHERE id = $1")
            .bind(instance)
            .bind(node)
            .execute(self.app.db())
            .await
            .unwrap();
        let mut vol = None;
        if volume {
            let r = self
                .app
                .call(
                    Method::POST,
                    &format!("/api/service-instances/{instance}/volume"),
                    Some(&self.admin),
                    Some(json!({ "mountPath": "/data" })),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
            vol = Some(
                sqlx::query_scalar("SELECT id FROM volumes WHERE service_instance_id = $1")
                    .bind(instance)
                    .fetch_one(self.app.db())
                    .await
                    .unwrap(),
            );
            let r = self
                .app
                .call(
                    Method::POST,
                    &format!("/api/service-instances/{instance}/tcp-proxy"),
                    Some(&self.admin),
                    Some(json!({ "targetPort": 5432 })),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        }
        let d = self
            .app
            .call(
                Method::POST,
                &format!("/api/service-instances/{instance}/deployments"),
                Some(&self.admin),
                None,
            )
            .await;
        assert_eq!(d.status, StatusCode::OK, "{:?}", d.json);
        let dep: Id = d.json["id"].as_str().unwrap().parse().unwrap();
        self.wait_active(dep).await;
        (instance, vol)
    }

    async fn wait_active(&self, deployment: Id) {
        let db = self.app.db().clone();
        wait_for("deployment active", || {
            let db = db.clone();
            async move {
                sqlx::query_scalar::<_, String>("SELECT status FROM deployments WHERE id = $1")
                    .bind(deployment)
                    .fetch_one(&db)
                    .await
                    .unwrap()
                    == "active"
            }
        })
        .await;
    }

    async fn node_of(&self, instance: Id) -> Option<Id> {
        sqlx::query_scalar("SELECT node_id FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(self.app.db())
            .await
            .unwrap()
    }

    /// Drive a move until it settles (what its job does every few seconds).
    async fn run_move(&self, id: Id) -> Poll {
        for _ in 0..600 {
            match moves::poll(&self.app.state, id).await.unwrap() {
                Poll::Pending => tokio::time::sleep(Duration::from_millis(50)).await,
                p => return p,
            }
        }
        panic!("the move did not settle");
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_worker_server_migrates_with_its_data(pool: PgPool) {
    let s = setup(pool).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let r = s
        .app
        .call(
            Method::PUT,
            &format!("/api/workspaces/{ws}/integrations/hetzner"),
            Some(&s.admin),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let old = s.node(&s.join_token().await, "worker-1").await;
    let old_server = Id::new();
    sqlx::query(
        "INSERT INTO provisioned_servers (id, workspace_id, provider, provider_id, name, location,
                                          server_type, monthly_price_cents, status, node_id)
         VALUES ($1, $2, 'hetzner', '4000', 'worker-1', 'hetzner:fsn1', 'cx22', 451, 'ready', $3)",
    )
    .bind(old_server)
    .bind(ws)
    .bind(old.id)
    .execute(s.app.db())
    .await
    .unwrap();
    let project = s.project(ws).await;
    let (db, vol) = s.service(&project, "db", old.id, true).await;
    let vol = vol.unwrap();
    let (api, _) = s.service(&project, "api", old.id, false).await;
    // What the database wrote.
    std::fs::create_dir_all(old.volume(vol).join("pg")).unwrap();
    // Incompressible and several flow-control windows long, so the copy
    // waits for credit relayed back from the new server.
    let data = noise(2_000_000);
    std::fs::write(old.volume(vol).join("pg/data.bin"), &data).unwrap();
    let old_port: i32 =
        sqlx::query_scalar("SELECT public_port FROM tcp_proxies WHERE service_instance_id = $1")
            .bind(db)
            .fetch_one(s.app.db())
            .await
            .unwrap();

    let p = s
        .app
        .call(
            Method::GET,
            &format!("/api/nodes/{}/migration-preview", old.id),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(p.status, StatusCode::OK, "{:?}", p.json);
    assert_eq!(p.json["services"].as_array().unwrap().len(), 2);
    assert_eq!(p.json["workspaceId"], ws.to_string());
    assert_eq!(p.json["main"], false);

    let body = json!({ "location": "hetzner:hel1", "serverType": "cx22" });
    let path = format!("/api/nodes/{}/migrate", old.id);
    let r = s
        .app
        .call(Method::POST, &path, Some(&s.admin), Some(body.clone()))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(r.json["step"], "join");
    let new_server: Id = r.json["targetServerId"].as_str().unwrap().parse().unwrap();
    // One at a time.
    let again = s
        .app
        .call(Method::POST, &path, Some(&s.admin), Some(body))
        .await;
    assert_eq!(again.status, StatusCode::CONFLICT, "{:?}", again.json);

    // The new server boots and joins with the token from its boot script.
    let user_data = s.mock.lock().unwrap().servers[0]["user_data"]
        .as_str()
        .unwrap()
        .to_owned();
    let token = user_data
        .split("--token '")
        .nth(1)
        .and_then(|t| t.split('\'').next())
        .unwrap()
        .to_owned();
    let new = s.node(&token, "worker-1-new").await;

    let st = s.app.state.clone();
    let mut done = false;
    for _ in 0..600 {
        modula_server::provision::track::poll_once(&st, new_server)
            .await
            .unwrap();
        if modula_server::migrate::server::poll(&st, id).await.unwrap() {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(done, "the migration settled");
    let m = s
        .app
        .call(
            Method::GET,
            &format!("/api/server-migrations/{id}"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(m.json["status"], "done", "{:#}", m.json);
    assert_eq!(m.json["targetNodeId"], new.id.to_string());
    assert_eq!(m.json["sourcePendingDeletion"], true);
    let moved = m.json["moves"].as_array().unwrap();
    assert_eq!(moved.len(), 2);
    assert_eq!(moved[0]["serviceInstanceId"], db.to_string(), "data first");
    assert!(moved.iter().all(|m| m["status"] == "done"));
    assert!(moved[0]["copiedBytes"].as_i64().unwrap() > 1_900_000);
    assert_eq!(m.json["tcpProxies"][0]["publicPort"], old_port);

    // Everything runs on the new server, the data with it.
    assert_eq!(s.node_of(db).await, Some(new.id));
    assert_eq!(s.node_of(api).await, Some(new.id));
    assert_eq!(
        std::fs::read(new.volume(vol).join("pg/data.bin")).unwrap(),
        data
    );
    let proxy_node: Id =
        sqlx::query_scalar("SELECT node_id FROM tcp_proxies WHERE service_instance_id = $1")
            .bind(db)
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert_eq!(proxy_node, new.id);
    let live_on_old: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM deployments WHERE node_id = $1 AND status IN ('active', 'deploying')",
    )
    .bind(old.id)
    .fetch_one(s.app.db())
    .await
    .unwrap();
    assert_eq!(live_on_old, 0);
    wait_for("old containers gone", || async {
        old.rt.running_deployments().is_empty()
    })
    .await;
    // The old server is only marked; deleting it is the user's call.
    let status: String = sqlx::query_scalar("SELECT status FROM provisioned_servers WHERE id = $1")
        .bind(old_server)
        .fetch_one(s.app.db())
        .await
        .unwrap();
    assert_eq!(status, "pending_deletion");
    // Nothing new lands on it.
    let (web, _) = s.service(&project, "web", new.id, false).await;
    sqlx::query("UPDATE service_instances SET node_id = NULL WHERE id = $1")
        .bind(web)
        .execute(s.app.db())
        .await
        .unwrap();
    let d = s
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{web}/deployments"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(d.json["nodeId"], new.id.to_string(), "{:?}", d.json);
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'server.migrate'")
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert_eq!(audited, 1);
    // A finished migration can't be aborted.
    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/server-migrations/{id}/abort"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_service_moves_with_its_volume_and_an_abort_puts_it_back(pool: PgPool) {
    let s = setup(pool).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let a = s.node(&s.join_token().await, "worker-a").await;
    let b = s.node(&s.join_token().await, "worker-b").await;
    let project = s.project(ws).await;
    let (db, vol) = s.service(&project, "db", a.id, true).await;
    let vol = vol.unwrap();
    std::fs::create_dir_all(a.volume(vol)).unwrap();
    std::fs::write(a.volume(vol).join("hello.txt"), b"from a").unwrap();

    let path = format!("/api/service-instances/{db}/move");
    let r = s
        .app
        .call(
            Method::POST,
            &path,
            Some(&s.admin),
            Some(json!({ "nodeId": a.id })),
        )
        .await;
    assert_eq!(
        r.status,
        StatusCode::CONFLICT,
        "already there: {:?}",
        r.json
    );
    let r = s
        .app
        .call(
            Method::POST,
            &path,
            Some(&s.admin),
            Some(json!({ "nodeId": b.id })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let mv: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(s.run_move(mv).await, Poll::Done);
    assert_eq!(s.node_of(db).await, Some(b.id));
    assert_eq!(
        std::fs::read(b.volume(vol).join("hello.txt")).unwrap(),
        b"from a"
    );
    let g = s
        .app
        .call(
            Method::GET,
            &format!("/api/instance-moves/{mv}"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(g.json["status"], "done");
    let labels: Vec<&str> = g.json["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["status"].as_str().unwrap())
        .collect();
    assert!(labels.iter().all(|l| *l == "done"), "{:#}", g.json);
    let served: Id = g.json["deploymentId"].as_str().unwrap().parse().unwrap();
    assert!(b.rt.running_deployments().contains(&served.to_string()));

    // Moving back: stopped on b, then aborted before the copy finishes.
    let r = s
        .app
        .call(
            Method::POST,
            &path,
            Some(&s.admin),
            Some(json!({ "nodeId": a.id })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let back: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        moves::poll(&s.app.state, back).await.unwrap(),
        Poll::Pending
    );
    let phase: String = sqlx::query_scalar("SELECT phase FROM instance_moves WHERE id = $1")
        .bind(back)
        .fetch_one(s.app.db())
        .await
        .unwrap();
    assert_eq!(phase, "stopping");
    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/instance-moves/{back}/abort"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "aborted");
    assert_eq!(s.node_of(db).await, Some(b.id), "it stays where it was");
    let again: Id = sqlx::query_scalar(
        "SELECT id FROM deployments WHERE service_instance_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(db)
    .fetch_one(s.app.db())
    .await
    .unwrap();
    assert_ne!(again, served);
    s.wait_active(again).await;
    assert!(b.rt.running_deployments().contains(&again.to_string()));
    assert_eq!(
        std::fs::read(b.volume(vol).join("hello.txt")).unwrap(),
        b"from a"
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn an_aborted_migration_puts_everything_back(pool: PgPool) {
    let s = setup(pool).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let r = s
        .app
        .call(
            Method::PUT,
            &format!("/api/workspaces/{ws}/integrations/hetzner"),
            Some(&s.admin),
            Some(json!({ "token": TOKEN })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let old = s.node(&s.join_token().await, "worker-1").await;
    let old_server = Id::new();
    sqlx::query(
        "INSERT INTO provisioned_servers (id, workspace_id, provider, provider_id, name, location,
                                          server_type, monthly_price_cents, status, node_id)
         VALUES ($1, $2, 'hetzner', '4000', 'worker-1', 'hetzner:fsn1', 'cx22', 451, 'ready', $3)",
    )
    .bind(old_server)
    .bind(ws)
    .bind(old.id)
    .execute(s.app.db())
    .await
    .unwrap();
    let project = s.project(ws).await;
    let (db, _) = s.service(&project, "db", old.id, true).await;
    let (api, _) = s.service(&project, "api", old.id, false).await;

    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/nodes/{}/migrate", old.id),
            Some(&s.admin),
            Some(json!({ "location": "hetzner:hel1", "serverType": "cx22" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    let new_server: Id = r.json["targetServerId"].as_str().unwrap().parse().unwrap();
    let user_data = s.mock.lock().unwrap().servers[0]["user_data"]
        .as_str()
        .unwrap()
        .to_owned();
    let token = user_data
        .split("--token '")
        .nth(1)
        .and_then(|t| t.split('\'').next())
        .unwrap()
        .to_owned();
    let _new = s.node(&token, "worker-1-new").await;

    // Until the database moved and the API is on its way.
    let st = s.app.state.clone();
    let mut reached = false;
    for _ in 0..600 {
        modula_server::provision::track::poll_once(&st, new_server)
            .await
            .unwrap();
        modula_server::migrate::server::poll(&st, id).await.unwrap();
        let api_moving: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM instance_moves WHERE migration_id = $1
                            AND service_instance_id = $2 AND status = 'running')",
        )
        .bind(id)
        .bind(api)
        .fetch_one(s.app.db())
        .await
        .unwrap();
        if api_moving {
            reached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(reached, "the second move started");

    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/server-migrations/{id}/abort"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "aborted", "{:#}", r.json);
    assert!(
        r.json["moves"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["status"] == "aborted")
    );
    assert_eq!(s.node_of(db).await, Some(old.id));
    assert_eq!(s.node_of(api).await, Some(old.id));
    let proxy_node: Id =
        sqlx::query_scalar("SELECT node_id FROM tcp_proxies WHERE service_instance_id = $1")
            .bind(db)
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert_eq!(proxy_node, old.id);
    let status: String = sqlx::query_scalar("SELECT status FROM provisioned_servers WHERE id = $1")
        .bind(old_server)
        .fetch_one(s.app.db())
        .await
        .unwrap();
    assert_eq!(status, "ready");
    let latest: Id = sqlx::query_scalar(
        "SELECT id FROM deployments WHERE service_instance_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(db)
    .fetch_one(s.app.db())
    .await
    .unwrap();
    s.wait_active(latest).await;
    assert!(old.rt.running_deployments().contains(&latest.to_string()));
    // The old server takes work again.
    let again = s
        .app
        .call(
            Method::GET,
            &format!("/api/nodes/{}/migration-preview", old.id),
            Some(&s.admin),
            None,
        )
        .await;
    assert!(again.json["activeMigrationId"].is_null());
}
