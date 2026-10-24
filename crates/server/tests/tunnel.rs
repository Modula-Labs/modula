//! Private tunnels end to end: the CLI's local listener → WebSocket on the real
//! HTTP server → the node's gRPC stream (mTLS) → a TCP connection the node
//! dials to the deployment's "container" (a local echo server here).

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_cli::api::api_error;
use modula_cli::config::Login;
use modula_cli::tunnel::{self, Target};
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use serde_json::json;
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

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

/// Echoes every connection; closes it on receiving `CLOSE`.
async fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    let n = match conn.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    if &buf[..n] == b"CLOSE" {
                        return;
                    }
                    if conn.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    port
}

struct Setup {
    app: TestApp,
    admin: String,
    api_url: String,
    token: String,
    _stop: CancellationToken,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self._stop.cancel();
    }
}

/// Server (HTTP + node port), one node on a fake runtime whose containers
/// "live" at 127.0.0.1, and an API token.
async fn setup(pool: PgPool) -> Setup {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    app.state
        .tunnels
        .set_check_every(Duration::from_millis(100));
    let stop = CancellationToken::new();

    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let grpc_port = grpc.local_addr().unwrap().port();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        grpc,
        stop.clone(),
    ));
    let http = TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    let t = app
        .call(Method::POST, "/api/nodes/join-tokens", Some(&admin), None)
        .await;
    let dir = std::env::temp_dir().join(format!("modula-tunnel-test-{}", Id::new()));
    let id = join(JoinArgs {
        server: &format!("https://localhost:{grpc_port}"),
        token: t.json["token"].as_str().unwrap(),
        fingerprint: t.json["fingerprint"].as_str().unwrap(),
        hostname: "worker-1".into(),
        dir: &dir,
    })
    .await
    .expect("join");
    let rt = Arc::new(FakeRuntime::new());
    *rt.address_override.lock().unwrap() = Some("127.0.0.1".parse().unwrap());
    let caps = modula_node::reconcile::Capabilities {
        edge: None,
        build: None,
        db: None,
        upgrade: None,
        mesh: None,
    };
    tokio::spawn(modula_node::session::run(dir, rt, caps, stop.child_token()));
    let node = id.meta.node_id;
    let st = app.state.clone();
    wait_for("node online", || {
        let st = st.clone();
        async move { st.hub.is_connected(node) }
    })
    .await;

    let r = app
        .call(
            Method::POST,
            "/api/account/tokens",
            Some(&admin),
            Some(json!({ "name": "cli" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    Setup {
        token: r.json["token"].as_str().unwrap().to_owned(),
        app,
        admin,
        api_url,
        _stop: stop,
    }
}

impl Setup {
    /// A service on `port`; returns (instance, deployment once active).
    async fn service(&self, port: u16, deploy: bool) -> (Id, Option<Id>) {
        let ws = self.app.create_workspace(&self.admin, "team").await;
        let p = self
            .app
            .call(
                Method::POST,
                &format!("/api/workspaces/{ws}/projects"),
                Some(&self.admin),
                Some(json!({ "name": "shop" })),
            )
            .await;
        let project = p.json["id"].as_str().unwrap().to_owned();
        let s = self
            .app
            .call(
                Method::POST,
                &format!("/api/projects/{project}/services"),
                Some(&self.admin),
                Some(json!({
                    "name": "db",
                    "config": { "source": { "type": "image", "image": "echo:1" }, "port": port },
                })),
            )
            .await;
        assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
        let instance: Id = s.json["instances"][0]["id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        if !deploy {
            return (instance, None);
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
        let deployment: Id = d.json["id"].as_str().unwrap().parse().unwrap();
        wait_for("deployment active", || async {
            let r = self
                .app
                .call(
                    Method::GET,
                    &format!("/api/deployments/{deployment}"),
                    Some(&self.admin),
                    None,
                )
                .await;
            r.json["status"] == "active"
        })
        .await;
        (instance, Some(deployment))
    }

    fn target(&self, instance: Id, token: &str) -> Target {
        let login = Login {
            url: self.api_url.parse().unwrap(),
            token: token.to_owned(),
            access: None,
        };
        Target::new(&login, &instance.to_string(), None).unwrap()
    }

    /// A local port tunnelled to `instance`.
    async fn local_port(&self, instance: Id) -> u16 {
        let target = self.target(instance, &self.token);
        target.check().await.expect("tunnel check");
        let listener = tunnel::listen(None).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(tunnel::serve(listener, target, false));
        port
    }

    async fn wait_closed(&self) {
        let st = self.app.state.clone();
        wait_for("tunnels closed", || {
            let st = st.clone();
            async move { st.tunnels.is_empty() }
        })
        .await;
    }
}

async fn read_exact_timeout(conn: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut got = vec![0u8; n];
    tokio::time::timeout(Duration::from_secs(10), conn.read_exact(&mut got))
        .await
        .expect("echo in time")
        .expect("echo");
    got
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn bytes_round_trip_and_closes_propagate(pool: PgPool) {
    let s = setup(pool).await;
    let echo = echo_server().await;
    let (instance, deployment) = s.service(echo, true).await;
    let local = s.local_port(instance).await;
    s.wait_closed().await; // the check's tunnel

    // More than a window each way, so credits must flow.
    let mut conn = TcpStream::connect(("127.0.0.1", local)).await.unwrap();
    let payload: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
    let (mut r, mut w) = conn.split();
    let (_, got) = tokio::join!(async { w.write_all(&payload).await.unwrap() }, async {
        let mut got = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(20), r.read_exact(&mut got))
            .await
            .expect("echo in time")
            .unwrap();
        got
    });
    assert!(got == payload, "echoed bytes differ");
    assert_eq!(s.app.state.tunnels.len(), 1);

    // The client hangs up: the tunnel closes everywhere and is audited.
    drop(conn);
    s.wait_closed().await;
    let closes = || async {
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT metadata FROM audit_log WHERE action = 'tunnel.close' ORDER BY id",
        )
        .fetch_all(s.app.db())
        .await
        .unwrap()
    };
    wait_for("close audited", || async { closes().await.len() == 2 }).await;
    let meta = closes().await.pop().unwrap();
    assert_eq!(meta["bytesUp"], 1_000_000);
    assert_eq!(meta["bytesDown"], 1_000_000);
    let opened: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'tunnel.open'")
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert_eq!(opened, 2);

    // The service hangs up: the local connection sees EOF.
    let mut conn = TcpStream::connect(("127.0.0.1", local)).await.unwrap();
    conn.write_all(b"ping").await.unwrap();
    assert_eq!(read_exact_timeout(&mut conn, 4).await, b"ping");
    conn.write_all(b"CLOSE").await.unwrap();
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), conn.read_to_end(&mut rest))
        .await
        .expect("EOF in time")
        .unwrap();
    s.wait_closed().await;

    // Removing the deployment closes open tunnels.
    let mut conn = TcpStream::connect(("127.0.0.1", local)).await.unwrap();
    conn.write_all(b"ping").await.unwrap();
    assert_eq!(read_exact_timeout(&mut conn, 4).await, b"ping");
    let r = s
        .app
        .call(
            Method::DELETE,
            &format!("/api/deployments/{}", deployment.unwrap()),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let mut rest = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), conn.read_to_end(&mut rest))
        .await
        .expect("EOF in time")
        .unwrap();
    s.wait_closed().await;
    // Audited right after the tunnel is forgotten.
    wait_for("close audited", || async {
        let reasons: Vec<String> = sqlx::query_scalar(
            "SELECT metadata->>'reason' FROM audit_log WHERE action = 'tunnel.close'",
        )
        .fetch_all(s.app.db())
        .await
        .unwrap();
        reasons.iter().any(|r| r == "the deployment was removed")
    })
    .await;
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn tunnels_explain_why_they_cannot_open(pool: PgPool) {
    let s = setup(pool).await;
    let (idle, _) = s.service(5432, false).await;
    let err = s.target(idle, &s.token).check().await.unwrap_err();
    let api = api_error(&err).expect("an API error");
    assert_eq!(api.status, 409);
    assert!(
        api.message.contains("no running deployment"),
        "{}",
        api.message
    );

    // Nothing listens on the port: the node's dial error comes back.
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_port = closed.local_addr().unwrap().port();
    drop(closed);
    let (instance, _) = s.service(dead_port, true).await;
    let err = s.target(instance, &s.token).check().await.unwrap_err();
    let api = api_error(&err).expect("an API error");
    assert!(
        api.message.contains("nothing is listening"),
        "{}",
        api.message
    );
    s.wait_closed().await;

    // Bad tokens never reach the node.
    let err = s.target(instance, "mdt_nope").check().await.unwrap_err();
    assert_eq!(api_error(&err).unwrap().status, 401);

    // A viewer can't open tunnels.
    let ws: Id = sqlx::query_scalar(
        "SELECT p.workspace_id FROM service_instances i JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(s.app.db())
    .await
    .unwrap();
    let (viewer_id, viewer) = s.app.user("viewer@example.com").await;
    s.app.add_member(ws, viewer_id, "viewer").await;
    let r = s
        .app
        .call(
            Method::POST,
            "/api/account/tokens",
            Some(&viewer),
            Some(json!({ "name": "cli" })),
        )
        .await;
    let viewer_token = r.json["token"].as_str().unwrap().to_owned();
    let err = s.target(instance, &viewer_token).check().await.unwrap_err();
    assert_eq!(api_error(&err).unwrap().status, 403);

    // A cookie session must come from our own origin (no cross-site WebSockets).
    let ws_status = |origin: Option<&'static str>| {
        let url = format!(
            "{}api/service-instances/{instance}/tunnel",
            s.api_url.replace("http://", "ws://")
        );
        let cookie = format!("{COOKIE}={}", s.admin);
        async move {
            use tokio_tungstenite::tungstenite::client::IntoClientRequest;
            let mut req = url.as_str().into_client_request().unwrap();
            req.headers_mut().insert("cookie", cookie.parse().unwrap());
            if let Some(o) = origin {
                req.headers_mut().insert("origin", o.parse().unwrap());
            }
            let tcp = TcpStream::connect(req.uri().authority().unwrap().as_str())
                .await
                .unwrap();
            match tokio_tungstenite::client_async(req, tcp).await {
                Ok(_) => 101,
                Err(tokio_tungstenite::tungstenite::Error::Http(res)) => res.status().as_u16(),
                Err(e) => panic!("{e}"),
            }
        }
    };
    assert_eq!(ws_status(None).await, 403);
    assert_eq!(ws_status(Some("https://evil.test")).await, 403);
    // From our origin it gets as far as the node (whose dial fails: nothing listens).
    assert_eq!(ws_status(Some(ORIGIN)).await, 409);
}
