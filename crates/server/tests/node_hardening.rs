//! Node hardening: revoked servers stay out, workspace-private servers are
//! only placed on by their workspace, and shipped log lines are bounded.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use modula_proto::node::v1 as pb;
use modula_server::deploy::placement;
use secrecy::ExposeSecret;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

struct Cluster {
    app: TestApp,
    admin: String,
    server_url: String,
    stop: CancellationToken,
}

impl Drop for Cluster {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn cluster(pool: PgPool) -> Cluster {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = CancellationToken::new();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        listener,
        stop.clone(),
    ));
    Cluster {
        app,
        admin,
        server_url: format!("https://localhost:{port}"),
        stop,
    }
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
    /// Join with `token` and run the node's session; returns its id.
    async fn start_node(&self, token: &str) -> Id {
        let fingerprint = self.app.state.pki().await.unwrap().fingerprint();
        let dir = std::env::temp_dir().join(format!("modula-node-test-{}", Id::new()));
        let id = join(JoinArgs {
            server: &self.server_url,
            token,
            fingerprint: &fingerprint,
            hostname: "worker-1".into(),
            dir: &dir,
        })
        .await
        .expect("join");
        let caps = modula_node::reconcile::Capabilities {
            edge: None,
            build: None,
            db: None,
            upgrade: None,
            mesh: None,
        };
        tokio::spawn(modula_node::session::run(
            dir,
            Arc::new(FakeRuntime::new()),
            caps,
            self.stop.child_token(),
        ));
        let node = id.meta.node_id;
        wait_for("node online", || async {
            self.app.state.hub.is_connected(node)
        })
        .await;
        node
    }

    async fn admin_token(&self) -> String {
        let r = self
            .app
            .call(
                Method::POST,
                "/api/nodes/join-tokens",
                Some(&self.admin),
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
        r.json["token"].as_str().unwrap().to_owned()
    }
}

async fn node_row(db: &PgPool, node: Id) -> (String, String, Option<Id>) {
    sqlx::query_as("SELECT status, cert_serial, workspace_id FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_revoked_node_cannot_attach_again(pool: PgPool) {
    let c = cluster(pool).await;
    let token = c.admin_token().await;
    let node = c.start_node(&token).await;
    let (status, serial, workspace) = node_row(c.app.db(), node).await;
    assert_eq!(status, "online");
    assert_eq!(workspace, None, "an admin's token enrolls a shared server");

    let r = c
        .app
        .call(
            Method::DELETE,
            &format!("/api/nodes/{node}"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let (status, tombstone, _) = node_row(c.app.db(), node).await;
    assert_eq!(status, "revoked");
    assert_eq!(
        tombstone,
        format!("revoked:{serial}"),
        "the old serial is gone"
    );

    // Revoking twice is a no-op, not a second tombstone.
    let r = c
        .app
        .call(
            Method::DELETE,
            &format!("/api/nodes/{node}"),
            Some(&c.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(node_row(c.app.db(), node).await.1, tombstone);

    // The node keeps retrying (backoff ≤ 8 s) and is turned away each time.
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!c.app.state.hub.is_connected(node));
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!c.app.state.hub.is_connected(node));
    assert_eq!(node_row(c.app.db(), node).await.0, "revoked");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_workspace_token_enrolls_a_private_server(pool: PgPool) {
    let c = cluster(pool).await;
    let ws = c.app.create_workspace(&c.admin, "ws").await;
    // What Hetzner provisioning mints (provision::api::provision).
    let (_, token, _) = modula_server::nodes::create_join_token(
        c.app.db(),
        None,
        Some(ws),
        time::Duration::minutes(5),
    )
    .await
    .unwrap();
    let node = c.start_node(token.expose_secret()).await;
    assert_eq!(node_row(c.app.db(), node).await.2, Some(ws));
}

async fn node(db: &PgPool, workspace: Option<Id>, status: &str) -> Id {
    let id = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, workspace_id, name, status, cert_serial, cert_expires_at, info)
         VALUES ($1, $2, 'worker', $3, $1::text, now() + interval '1 day',
                 '{\"cpus\": 8, \"memoryBytes\": 17179869184}'::jsonb)",
    )
    .bind(id)
    .bind(workspace)
    .bind(status)
    .execute(db)
    .await
    .unwrap();
    id
}

async fn set_node(db: &PgPool, instance: Id, node: Id) {
    sqlx::query("UPDATE service_instances SET node_id = $2 WHERE id = $1")
        .bind(instance)
        .bind(node)
        .execute(db)
        .await
        .unwrap();
}

async fn choose(db: &PgPool, instance: Id) -> Result<Id, modula_server::error::AppError> {
    let mut conn = db.acquire().await.unwrap();
    placement::choose(&mut conn, instance).await
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn placement_skips_other_workspaces_servers(pool: PgPool) {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "ws").await;
    let other = app.create_workspace(&admin, "other").await;
    // seed_chain's node is shared (and offline, with a live deployment).
    let chain = app.seed_chain(ws).await;
    let db = app.db();
    let foreign = node(db, Some(other), "online").await;

    // A sticky node from another workspace is not reused, even online.
    set_node(db, chain.instance, foreign).await;
    assert_eq!(choose(db, chain.instance).await.unwrap(), chain.node);

    // The workspace's own online server wins over the offline shared one.
    let own = node(db, Some(ws), "online").await;
    set_node(db, chain.instance, foreign).await;
    assert_eq!(choose(db, chain.instance).await.unwrap(), own);

    // Only the foreign server left: nowhere to deploy.
    sqlx::query("UPDATE nodes SET status = 'revoked' WHERE id IN ($1, $2)")
        .bind(chain.node)
        .bind(own)
        .execute(db)
        .await
        .unwrap();
    set_node(db, chain.instance, foreign).await;
    assert!(choose(db, chain.instance).await.is_err());

    // The suggester's room follows the same rule.
    let shared = node(db, None, "online").await;
    let room: Vec<Id> = modula_server::provision::suggest::room(db, ws, None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.node_id)
        .collect();
    assert_eq!(room, [shared]);
    let room: Vec<Id> = modula_server::provision::suggest::room(db, other, None)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.node_id)
        .collect();
    assert_eq!(room, [foreign, shared]);
}

#[test]
fn shipped_log_lines_are_bounded() {
    use modula_server::logs::{MAX_BATCH_BYTES, MAX_LINE_BYTES, clamp, truncate_line};

    // Never splits a character: "é" is two bytes.
    assert_eq!(truncate_line("aé", 2), "a");
    assert_eq!(truncate_line("aé", 3), "aé");
    assert_eq!(truncate_line("short", 100), "short");

    let line = |text: String| pb::ShippedLine {
        deployment_id: "d".into(),
        at_unix_ms: 0,
        stderr: false,
        text,
    };
    let b = pb::LogBatch {
        lines: vec![
            line(format!("x\0{}", "é".repeat(MAX_LINE_BYTES))),
            line("ok".into()),
        ],
        dropped: 0,
    };
    let kept = clamp(&b);
    assert_eq!(kept.len(), 2);
    assert!(kept[0].1.len() <= MAX_LINE_BYTES);
    assert!(kept[0].1.starts_with("xé"), "NULs are stripped");
    assert_eq!(kept[1].1, "ok");

    // A batch over the byte cap loses its tail.
    let big = pb::LogBatch {
        lines: (0..1000)
            .map(|_| line("y".repeat(MAX_LINE_BYTES)))
            .collect(),
        dropped: 0,
    };
    let kept = clamp(&big);
    assert_eq!(kept.len(), MAX_BATCH_BYTES / MAX_LINE_BYTES);
}
