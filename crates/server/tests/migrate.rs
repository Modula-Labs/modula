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
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const TOKEN: &str = "hetzner-test-token-0123456789abcdefghijklmnopqrstuvwxyz";

/// Just enough of the Hetzner Cloud API to create a server.
#[derive(Default)]
struct Mock {
    servers: Vec<Value>,
    /// What GET /v1/firewalls lists.
    firewalls: Vec<Value>,
    /// Every set_rules body.
    rule_sets: Vec<Value>,
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
            get(|State(m): State<Shared>| async move {
                Json(page(json!(m.lock().unwrap().firewalls), "firewalls"))
            }).post(|Json(mut body): Json<Value>| async move {
                body["id"] = json!(100);
                (StatusCode::CREATED, Json(json!({"firewall": body, "actions": []})))
            }),
        )
        .route(
            "/v1/firewalls/{id}/actions/set_rules",
            post(|State(m): State<Shared>, Json(body): Json<Value>| async move {
                m.lock().unwrap().rule_sets.push(body);
                (StatusCode::CREATED, Json(json!({"actions": []})))
            }),
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

mod ec2 {
    #![allow(dead_code)]
    use super::*;
    use axum::http::HeaderMap;
    use axum::response::{IntoResponse, Response};
    use base64::Engine;
    use std::collections::{BTreeMap, HashMap};

    include!("common/ec2_mock.rs");
}

/// The request body of a migration to a new server.
fn new_at(provider: &str, location: &str, server_type: &str) -> Value {
    json!({ "target": { "kind": "new", "provider": provider, "location": location, "serverType": server_type } })
}

fn boot_token(user_data: &str) -> String {
    user_data
        .split("--token '")
        .nth(1)
        .and_then(|t| t.split('\'').next())
        .unwrap()
        .to_owned()
}

struct Setup {
    app: TestApp,
    admin: String,
    grpc_port: u16,
    mock: Shared,
    aws: ec2::Shared,
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

/// Each test opens its own pool instead of taking one from sqlx's shared
/// 20-connection budget: these tests run nodes, gRPC and pollers, and a dozen
/// of them in parallel need more than that.
async fn setup(db: PgConnectOptions) -> Setup {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(db)
        .await
        .unwrap();
    let (url, mock) = mock_hetzner().await;
    let mut cfg = config();
    cfg.hetzner_api = url;
    let (aws_url, aws) = ec2::mock_aws().await;
    cfg.aws_api = Some(aws_url);
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
        aws,
        stop,
    }
}

impl Setup {
    async fn connect_providers(&self, ws: Id) {
        for (kind, body) in [
            ("hetzner", json!({ "token": TOKEN })),
            (
                "aws",
                json!({ "accessKeyId": ec2::KEY_ID, "secretAccessKey": ec2::SECRET }),
            ),
        ] {
            let r = self
                .app
                .call(
                    Method::PUT,
                    &format!("/api/workspaces/{ws}/integrations/{kind}"),
                    Some(&self.admin),
                    Some(body),
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        }
    }

    /// Record `node` as a server at a provider, ready.
    async fn link(
        &self,
        ws: Id,
        node: Id,
        provider: &str,
        pid: &str,
        location: &str,
        ty: &str,
    ) -> Id {
        let id = Id::new();
        sqlx::query(
            "INSERT INTO provisioned_servers (id, workspace_id, provider, provider_id, name, location,
                                              server_type, monthly_price_cents, status, node_id)
             VALUES ($1, $2, $3, $4, 'worker-1', $5, $6, 451, 'ready', $7)",
        )
        .bind(id)
        .bind(ws)
        .bind(provider)
        .bind(pid)
        .bind(location)
        .bind(ty)
        .bind(node)
        .execute(self.app.db())
        .await
        .unwrap();
        id
    }

    /// A joined server fit to be an existing target: public IP and capacity.
    async fn existing(&self, name: &str, ip: &str, workspace: Option<Id>) -> Node {
        let n = self.node(&self.join_token().await, name).await;
        sqlx::query(
            "UPDATE nodes SET public_ip = $2, workspace_id = $3,
                    info = info || '{\"cpus\": 8, \"memoryBytes\": 17179869184}'::jsonb
             WHERE id = $1",
        )
        .bind(n.id)
        .bind(ip)
        .bind(workspace)
        .execute(self.app.db())
        .await
        .unwrap();
        n
    }

    async fn start(&self, node: Id, body: Value) -> Resp {
        self.app
            .call(
                Method::POST,
                &format!("/api/nodes/{node}/migrate"),
                Some(&self.admin),
                Some(body),
            )
            .await
    }

    /// Poll migration `id` (and the new server, if any) until it settles.
    async fn settle(&self, id: Id, new_server: Option<Id>) -> Value {
        let st = self.app.state.clone();
        for _ in 0..600 {
            if let Some(server) = new_server {
                modula_server::provision::track::poll_once(&st, server)
                    .await
                    .unwrap();
            }
            if modula_server::migrate::server::poll(&st, id).await.unwrap() {
                return self
                    .app
                    .call(
                        Method::GET,
                        &format!("/api/server-migrations/{id}"),
                        Some(&self.admin),
                        None,
                    )
                    .await
                    .json;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("the migration did not settle");
    }

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

/// `${{db.MODULA_TCP_PROXY_DOMAIN}}` is resolved at deploy time: a service
/// deployed before its database's proxy moved keeps the old address.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn services_holding_a_moved_tcp_address_are_listed(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let old = s.node(&s.join_token().await, "worker-1").await;
    let new = s.node(&s.join_token().await, "worker-2").await;
    sqlx::query("UPDATE nodes SET public_ip = '203.0.113.5' WHERE id = $1")
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
            Method::PATCH,
            &format!("/api/service-instances/{api}/variables"),
            Some(&s.admin),
            Some(json!({ "set": { "DB_HOST": "${{db.MODULA_TCP_PROXY_DOMAIN}}" } })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let d = s
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{api}/deployments"),
            Some(&s.admin),
            None,
        )
        .await;
    s.wait_active(d.json["id"].as_str().unwrap().parse().unwrap())
        .await;

    let migration = Id::new();
    sqlx::query(
        "INSERT INTO server_migrations (id, workspace_id, source_node_id, source_name, target_location,
                                        target_server_type, target_node_id, status, step, steps, source_ip)
         VALUES ($1, $2, $3, 'worker-1', 'hetzner:hel1', 'cx22', $4, 'running', 'move', '[]', '203.0.113.5')",
    )
    .bind(migration)
    .bind(ws)
    .bind(old.id)
    .bind(new.id)
    .execute(s.app.db())
    .await
    .unwrap();
    let mv = moves::create(
        s.app.db(),
        moves::NewMove {
            workspace: ws,
            instance: db,
            migration: Some(migration),
            kind: "primary",
            from: Some(old.id),
            to: new.id,
            created_by: None,
            status: "running",
        },
    )
    .await
    .unwrap();
    assert_eq!(s.run_move(mv).await, Poll::Done);
    sqlx::query("UPDATE server_migrations SET step = 'dns' WHERE id = $1")
        .bind(migration)
        .execute(s.app.db())
        .await
        .unwrap();
    assert!(
        modula_server::migrate::server::poll(&s.app.state, migration)
            .await
            .unwrap()
    );
    let m = s
        .app
        .call(
            Method::GET,
            &format!("/api/server-migrations/{migration}"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(m.json["status"], "done", "{:#}", m.json);
    let stale = m.json["staleTcp"].as_array().unwrap();
    assert_eq!(stale.len(), 1, "{:#}", m.json);
    assert_eq!(stale[0]["service"], "api");
    assert_eq!(stale[0]["variables"], json!(["DB_HOST"]));
}

/// A changed public address moves DNS: the node's services at once, a main
/// server's own names through a delayed job.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_changed_public_address_queues_the_dns_resync(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let node = s.node(&s.join_token().await, "worker-1").await;
    let project = s.project(ws).await;
    let (api, _) = s.service(&project, "api", node.id, false).await;
    sqlx::query(
        "INSERT INTO domains (id, service_instance_id, hostname, kind, status, verification_token)
         VALUES ($1, $2, 'www.shop.test', 'custom', 'verified', 't')",
    )
    .bind(Id::new())
    .bind(api)
    .execute(s.app.db())
    .await
    .unwrap();
    let count = |kind: &'static str| {
        let db = s.app.db().clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM jobs WHERE kind = $1")
                .bind(kind)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    let before = count(modula_server::domains::RESYNC_JOB).await;
    modula_server::domains::on_public_ip_changed(&s.app.state, node.id, "203.0.113.1", false)
        .await
        .unwrap();
    assert_eq!(count(modula_server::domains::RESYNC_JOB).await, before + 1);
    assert_eq!(count(modula_server::domains::REPOINT_MAIN_JOB).await, 0);
    for _ in 0..2 {
        modula_server::domains::on_public_ip_changed(&s.app.state, node.id, "203.0.113.1", true)
            .await
            .unwrap();
    }
    assert_eq!(
        count(modula_server::domains::REPOINT_MAIN_JOB).await,
        1,
        "one per node, however often it reconnects"
    );
}

/// Whoever the `*.apps` wildcard points at must keep answering for a moved
/// service until DNS follows: the old server forwards over the mesh to the
/// new replica (never to anything of its own, so no stale data).
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn moved_services_stay_routed_through_the_old_server(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let old = s.node(&s.join_token().await, "worker-1").await;
    let new = s.node(&s.join_token().await, "worker-2").await;
    let project = s.project(ws).await;
    let (api, _) = s.service(&project, "api", old.id, false).await;
    let (db, _) = s.service(&project, "db", old.id, true).await;
    let mut hosts = Vec::new();
    for i in [api, db] {
        let r = s
            .app
            .call(
                Method::POST,
                &format!("/api/service-instances/{i}/domains"),
                Some(&s.admin),
                Some(json!({})),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        hosts.push(r.json["hostname"].as_str().unwrap().to_owned());
    }
    // A custom domain CNAME'd to the generated one arrives the same way.
    sqlx::query(
        "INSERT INTO domains (id, service_instance_id, hostname, kind, status, verification_token)
         VALUES ($1, $2, 'www.shop.test', 'custom', 'verified', 't')",
    )
    .bind(Id::new())
    .bind(api)
    .execute(s.app.db())
    .await
    .unwrap();
    hosts.push("www.shop.test".to_owned());
    let migration = Id::new();
    sqlx::query(
        "INSERT INTO server_migrations (id, workspace_id, source_node_id, source_name, target_location,
                                        target_server_type, target_node_id, status, step, steps)
         VALUES ($1, $2, $3, 'worker-1', 'hetzner:hel1', 'cx22', $4, 'running', 'move', '[]')",
    )
    .bind(migration)
    .bind(ws)
    .bind(old.id)
    .bind(new.id)
    .execute(s.app.db())
    .await
    .unwrap();
    let st = s.app.state.clone();
    for i in [db, api] {
        let mv = moves::create(
            s.app.db(),
            moves::NewMove {
                workspace: ws,
                instance: i,
                migration: Some(migration),
                kind: "primary",
                from: Some(old.id),
                to: new.id,
                created_by: None,
                status: "running",
            },
        )
        .await
        .unwrap();
        assert_eq!(s.run_move(mv).await, Poll::Done);
    }
    // In the fake runtime no mesh address is assigned.
    sqlx::query("UPDATE deployment_replicas SET mesh_ip = '10.80.9.2', status = 'active' WHERE node_id = $1")
        .bind(new.id)
        .execute(s.app.db())
        .await
        .unwrap();

    let routed = |ds: &modula_proto::node::v1::DesiredState, host: &str| {
        ds.routes
            .iter()
            .find(|r| r.hostname == host)
            .map(|r| r.remote_upstreams.clone())
    };
    let ds = modula_server::deploy::desired::build(&st, old.id)
        .await
        .unwrap();
    for h in &hosts {
        assert_eq!(
            routed(&ds, h),
            Some(vec!["10.80.9.2:8080".to_owned()]),
            "{h} on the old server: {:?}",
            ds.routes
        );
    }

    // An aborted migration puts everything back: no forwarding.
    sqlx::query("UPDATE server_migrations SET status = 'aborted' WHERE id = $1")
        .bind(migration)
        .execute(s.app.db())
        .await
        .unwrap();
    let ds = modula_server::deploy::desired::build(&st, old.id)
        .await
        .unwrap();
    assert_eq!(routed(&ds, &hosts[0]), None);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_worker_server_migrates_with_its_data(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
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

    let body = new_at("hetzner", "hetzner:hel1", "cx22");
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
async fn a_service_moves_with_its_volume_and_an_abort_puts_it_back(
    _: PgPoolOptions,
    db: PgConnectOptions,
) {
    let s = setup(db).await;
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
async fn an_aborted_migration_puts_everything_back(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
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
            Some(new_at("hetzner", "hetzner:hel1", "cx22")),
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

/// Hetzner to EC2: the root disk asked for is used, the Elastic IP (known
/// only once associated) reaches the Hetzner firewall before the first
/// move, and the DNS-step bookkeeping is set.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_hetzner_server_migrates_to_ec2(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.connect_providers(ws).await;
    s.mock.lock().unwrap().firewalls.push(json!({
        "id": 7, "name": "modula-workers", "rules": [], "labels": {"modula": "1"}
    }));
    let old = s.existing("worker-1", "203.0.113.5", Some(ws)).await;
    let old_server = s
        .link(ws, old.id, "hetzner", "4000", "hetzner:fsn1", "cx22")
        .await;
    let project = s.project(ws).await;
    let (api, _) = s.service(&project, "api", old.id, false).await;

    let mut body = new_at("aws", "aws:us-east-1", "t3.small");
    body["target"]["diskGb"] = json!(120);
    body["dnsSkip"] = json!(["rec1"]);
    let r = s.start(old.id, body).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    let new_server: Id = r.json["targetServerId"].as_str().unwrap().parse().unwrap();
    assert_eq!(r.json["targetExisting"], false);
    let (ip, skip): (Option<String>, Value) =
        sqlx::query_as("SELECT source_ip, dns_skip FROM server_migrations WHERE id = $1")
            .bind(id)
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert_eq!(ip.as_deref(), Some("203.0.113.5"));
    assert_eq!(skip, json!(["rec1"]));
    let row_ip: Option<String> =
        sqlx::query_scalar("SELECT ipv4 FROM provisioned_servers WHERE id = $1")
            .bind(new_server)
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert_eq!(row_ip, None, "no address until it is associated");

    let user_data = {
        let mut m = s.aws.lock().unwrap();
        assert_eq!(m.instances[0].root_gb, 120);
        m.instances[0].state = "running".into();
        m.instances[0].user_data.clone()
    };
    let new = s.node(&boot_token(&user_data), "worker-1-new").await;

    // Admitted before anything moves.
    let st = s.app.state.clone();
    for _ in 0..200 {
        modula_server::provision::track::poll_once(&st, new_server)
            .await
            .unwrap();
        modula_server::migrate::server::poll(&st, id).await.unwrap();
        let step: String = sqlx::query_scalar("SELECT step FROM server_migrations WHERE id = $1")
            .bind(id)
            .fetch_one(s.app.db())
            .await
            .unwrap();
        if step == "move" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let rules = serde_json::to_string(&s.mock.lock().unwrap().rule_sets).unwrap();
    assert!(rules.contains(ec2::EIP), "{rules}");

    let m = s.settle(id, Some(new_server)).await;
    assert_eq!(m["status"], "done", "{m:#}");
    assert_eq!(m["targetNodeId"], new.id.to_string());
    assert_eq!(m["sourcePendingDeletion"], true);
    assert_eq!(s.node_of(api).await, Some(new.id));
    assert_eq!(m["sourceServerId"], old_server.to_string());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn an_ec2_server_migrates_to_hetzner(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.connect_providers(ws).await;
    let old = s.existing("worker-1", "198.51.100.5", Some(ws)).await;
    s.link(
        ws,
        old.id,
        "aws",
        "us-east-1/i-99",
        "aws:us-east-1",
        "t3.small",
    )
    .await;
    let project = s.project(ws).await;
    let (api, _) = s.service(&project, "api", old.id, false).await;

    // A location must be the provider's.
    let wrong = s.start(old.id, new_at("aws", "hetzner:hel1", "cx22")).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST, "{:?}", wrong.json);

    let r = s
        .start(old.id, new_at("hetzner", "hetzner:hel1", "cx22"))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    let new_server: Id = r.json["targetServerId"].as_str().unwrap().parse().unwrap();
    let user_data = s.mock.lock().unwrap().servers[0]["user_data"]
        .as_str()
        .unwrap()
        .to_owned();
    let new = s.node(&boot_token(&user_data), "worker-1-new").await;
    let m = s.settle(id, Some(new_server)).await;
    assert_eq!(m["status"], "done", "{m:#}");
    assert_eq!(s.node_of(api).await, Some(new.id));
    assert_eq!(m["targetLocation"]["id"], "hetzner:hel1");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_server_migrates_to_an_existing_one(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.connect_providers(ws).await;
    let other = s.app.create_workspace(&s.admin, "other").await;
    let old = s.existing("worker-1", "203.0.113.5", Some(ws)).await;
    s.link(ws, old.id, "hetzner", "4000", "hetzner:fsn1", "cx22")
        .await;
    let target = s.existing("shared-1", "203.0.113.6", None).await;
    let private = s.existing("theirs", "203.0.113.7", Some(other)).await;
    let project = s.project(ws).await;
    let (api, _) = s.service(&project, "api", old.id, false).await;

    let existing = |n: Id| json!({ "target": { "kind": "existing", "nodeId": n } });
    // Not itself, nor another workspace's server.
    let r = s.start(old.id, existing(old.id)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    let r = s.start(old.id, existing(private.id)).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    // Nor one too small.
    sqlx::query(
        "UPDATE nodes SET info = info || '{\"memoryBytes\": 1048576}'::jsonb WHERE id = $1",
    )
    .bind(target.id)
    .execute(s.app.db())
    .await
    .unwrap();
    let r = s.start(old.id, existing(target.id)).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(
        r.code() != "" && r.json.to_string().contains("room"),
        "{:?}",
        r.json
    );
    sqlx::query(
        "UPDATE nodes SET info = info || '{\"memoryBytes\": 17179869184}'::jsonb WHERE id = $1",
    )
    .bind(target.id)
    .execute(s.app.db())
    .await
    .unwrap();

    let p = s
        .app
        .call(
            Method::GET,
            &format!(
                "/api/nodes/{}/migration-preview?targetNodeId={}",
                old.id, target.id
            ),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(p.status, StatusCode::OK, "{:?}", p.json);
    let listed = p.json["existingTargets"].as_array().unwrap();
    assert!(
        listed
            .iter()
            .any(|t| t["nodeId"] == target.id.to_string() && t["blocker"].is_null())
    );
    assert!(!listed.iter().any(|t| t["nodeId"] == private.id.to_string()));

    let r = s.start(old.id, existing(target.id)).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["targetExisting"], true);
    assert!(r.json["targetServerId"].is_null());
    assert_eq!(r.json["targetNodeId"], target.id.to_string());
    assert_eq!(r.json["targetName"], "shared-1");
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();

    // The target can't start a migration of its own meanwhile.
    let r = s
        .start(
            target.id,
            json!({ "workspaceId": ws, "target": { "kind": "existing", "nodeId": old.id } }),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);

    let m = s.settle(id, None).await;
    assert_eq!(m["status"], "done", "{m:#}");
    assert_eq!(m["sourcePendingDeletion"], true);
    assert_eq!(s.node_of(api).await, Some(target.id));
    let moves = m["moves"].as_array().unwrap();
    assert_eq!(moves[0]["fromNodeId"], old.id.to_string());
    assert_eq!(moves[0]["migrationId"], id.to_string());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn aborting_keeps_an_existing_target(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.connect_providers(ws).await;
    let old = s.existing("worker-1", "203.0.113.5", Some(ws)).await;
    s.link(ws, old.id, "hetzner", "4000", "hetzner:fsn1", "cx22")
        .await;
    let target = s.existing("worker-2", "203.0.113.6", Some(ws)).await;
    let target_server = Id::new();
    sqlx::query(
        "INSERT INTO provisioned_servers (id, workspace_id, provider, provider_id, name, location,
                                          server_type, monthly_price_cents, status, node_id)
         VALUES ($1, $2, 'hetzner', '4001', 'worker-2', 'hetzner:hel1', 'cx22', 451, 'ready', $3)",
    )
    .bind(target_server)
    .bind(ws)
    .bind(target.id)
    .execute(s.app.db())
    .await
    .unwrap();
    let project = s.project(ws).await;
    let (api, _) = s.service(&project, "api", old.id, false).await;

    let r = s
        .start(
            old.id,
            json!({ "target": { "kind": "existing", "nodeId": target.id } }),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(r.json["targetServerId"].is_null(), "not ours to delete");
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    modula_server::migrate::server::poll(&s.app.state, id)
        .await
        .unwrap();

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
    assert_eq!(s.node_of(api).await, Some(old.id));
    let (status, revoked): (String, bool) = sqlx::query_as(
        "SELECT p.status, n.status = 'revoked' FROM provisioned_servers p
         JOIN nodes n ON n.id = p.node_id WHERE p.id = $1",
    )
    .bind(target_server)
    .fetch_one(s.app.db())
    .await
    .unwrap();
    assert_eq!(status, "ready");
    assert!(!revoked);
    assert!(s.mock.lock().unwrap().servers.is_empty(), "nothing created");
}

/// A server joined by hand: the request names the workspace paying, there
/// is no old provider server to mark.
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_hand_added_server_migrates_for_the_named_workspace(
    _: PgPoolOptions,
    db: PgConnectOptions,
) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.connect_providers(ws).await;
    let old = s.existing("by-hand", "203.0.113.5", None).await;
    let project = s.project(ws).await;
    let (api, _) = s.service(&project, "api", old.id, false).await;

    let p = s
        .app
        .call(
            Method::GET,
            &format!("/api/nodes/{}/migration-preview", old.id),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(p.json["workspaceRequired"], true, "{:#}", p.json);

    let unnamed = s
        .start(old.id, new_at("hetzner", "hetzner:hel1", "cx22"))
        .await;
    assert_eq!(
        unnamed.status,
        StatusCode::BAD_REQUEST,
        "{:?}",
        unnamed.json
    );

    let mut body = new_at("hetzner", "hetzner:hel1", "cx22");
    body["workspaceId"] = json!(ws);
    let r = s.start(old.id, body).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(r.json["sourceServerId"].is_null());
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    let new_server: Id = r.json["targetServerId"].as_str().unwrap().parse().unwrap();
    let user_data = s.mock.lock().unwrap().servers[0]["user_data"]
        .as_str()
        .unwrap()
        .to_owned();
    let new = s.node(&boot_token(&user_data), "by-hand-new").await;
    let m = s.settle(id, Some(new_server)).await;
    assert_eq!(m["status"], "done", "{m:#}");
    assert_eq!(m["sourcePendingDeletion"], false);
    assert_eq!(s.node_of(api).await, Some(new.id));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn another_workspaces_server_is_refused(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let other = s.app.create_workspace(&s.admin, "other").await;
    s.connect_providers(ws).await;
    let theirs = s.existing("theirs", "203.0.113.5", Some(other)).await;
    let mut body = new_at("hetzner", "hetzner:hel1", "cx22");
    body["workspaceId"] = json!(ws);
    let r = s.start(theirs.id, body.clone()).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(s.mock.lock().unwrap().servers.is_empty());

    // An instance admin who is no admin of the workspace named can't either.
    let shared = s.existing("shared", "203.0.113.6", None).await;
    let (user, cookie) = s.app.user("root2@example.com").await;
    sqlx::query("UPDATE users SET is_instance_admin = true WHERE id = $1")
        .bind(user)
        .execute(s.app.db())
        .await
        .unwrap();
    let r = s
        .app
        .call(
            Method::POST,
            &format!("/api/nodes/{}/migrate", shared.id),
            Some(&cookie),
            Some(body),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{:?}", r.json);

    // Another tenant's services can't be taken along.
    let project = s.project(other).await;
    s.service(&project, "api", shared.id, false).await;
    let mut body = new_at("hetzner", "hetzner:hel1", "cx22");
    body["workspaceId"] = json!(ws);
    let r = s.start(shared.id, body).await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(s.mock.lock().unwrap().servers.is_empty());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn volumes_that_cannot_fit_are_refused(_: PgPoolOptions, db: PgConnectOptions) {
    let s = setup(db).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    s.connect_providers(ws).await;
    let old = s.existing("worker-1", "203.0.113.5", Some(ws)).await;
    s.link(ws, old.id, "hetzner", "4000", "hetzner:fsn1", "cx22")
        .await;
    let project = s.project(ws).await;
    let (_, vol) = s.service(&project, "db", old.id, true).await;
    sqlx::query(
        "INSERT INTO volume_usage (volume_id, node_id, used_bytes) VALUES ($1, $2, 90000000000)
         ON CONFLICT (volume_id) DO UPDATE SET used_bytes = EXCLUDED.used_bytes",
    )
    .bind(vol.unwrap())
    .bind(old.id)
    .execute(s.app.db())
    .await
    .unwrap();

    // EC2's default root disk is 80 GB.
    let p = s
        .app
        .call(
            Method::GET,
            &format!(
                "/api/nodes/{}/migration-preview?location=aws:us-east-1&serverType=t3.small",
                old.id
            ),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(p.status, StatusCode::OK, "{:?}", p.json);
    assert!(
        p.json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == "disk" && w["blocking"] == true),
        "{:#}",
        p.json["warnings"]
    );
    let r = s
        .start(old.id, new_at("aws", "aws:us-east-1", "t3.small"))
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(s.aws.lock().unwrap().instances.is_empty());

    let mut body = new_at("aws", "aws:us-east-1", "t3.small");
    body["target"]["diskGb"] = json!(200);
    let r = s.start(old.id, body).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}
