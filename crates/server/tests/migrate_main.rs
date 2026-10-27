//! Migrating the main server, the in-process parts: maintenance gating, the
//! single-use bundle over the node's identity, the address change on real
//! node sessions, and the step machine from `promote` to the hand-over with
//! a node whose installer is a fake.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, Request, StatusCode, header};
use common::*;
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use modula_server::migrate::{maintenance, promote};
use serde_json::json;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

struct Setup {
    app: TestApp,
    admin: String,
    port: u16,
    stop: CancellationToken,
    dir: std::path::PathBuf,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn fake_promoter() -> Arc<modula_node::promote::FakePromoter> {
    static P: std::sync::OnceLock<Arc<modula_node::promote::FakePromoter>> =
        std::sync::OnceLock::new();
    P.get_or_init(|| {
        let p = Arc::new(modula_node::promote::FakePromoter::default());
        modula_node::promote::register(p.clone());
        p
    })
    .clone()
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
    fake_promoter();
    let dir = std::env::temp_dir().join(format!("modula-main-migration-{}", Id::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let grpc = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config();
    cfg.node_listen = grpc.local_addr().unwrap();
    cfg.master_key_file = dir.join("master.key");
    std::fs::write(&cfg.master_key_file, b"not really a key").unwrap();
    std::fs::create_dir_all(cfg.uploads_dir.join("ws")).unwrap();
    std::fs::write(cfg.uploads_dir.join("ws/src.tar.gz"), b"source").unwrap();
    // pg_dump needs this test's own database.
    let name: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&pool)
        .await
        .unwrap();
    let mut url: url::Url = std::env::var("DATABASE_URL").unwrap().parse().unwrap();
    url.set_path(&format!("/{name}"));
    cfg.database_url = url.to_string();
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    let stop = CancellationToken::new();
    let port = grpc.local_addr().unwrap().port();
    tokio::spawn(modula_server::nodes::serve(
        app.state.clone(),
        grpc,
        stop.clone(),
    ));
    Setup {
        app,
        admin,
        port,
        stop,
        dir,
    }
}

impl Setup {
    async fn node(&self, name: &str) -> (Id, std::path::PathBuf) {
        let t = self
            .app
            .call(
                Method::POST,
                "/api/nodes/join-tokens",
                Some(&self.admin),
                None,
            )
            .await;
        let token = t.json["token"].as_str().unwrap().to_owned();
        let fingerprint = self.app.state.pki().await.unwrap().fingerprint();
        let dir = self.dir.join(name);
        let id = join(JoinArgs {
            server: &format!("https://localhost:{}", self.port),
            token: &token,
            fingerprint: &fingerprint,
            hostname: name.into(),
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
            access_log: None,
            wake: None,
            functions: None,
        };
        tokio::spawn(modula_node::session::run(
            dir.clone(),
            Arc::new(FakeRuntime::new()),
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
        (node, dir)
    }

    /// A main migration from `source` to `target`, at step promote.
    async fn migration(&self, source: Option<Id>, target: Id, phase: &str) -> Id {
        let ws = self
            .app
            .create_workspace(&self.admin, &format!("w{}", &Id::new().to_string()[..6]))
            .await;
        let id = Id::new();
        sqlx::query(
            "INSERT INTO server_migrations (id, workspace_id, source_node_id, source_name, target_location,
                                            target_server_type, target_node_id, status, step, main,
                                            promote_phase, promote_phase_at, steps)
             VALUES ($1, $2, $3, 'main', 'hetzner:hel1', 'cx22', $4, 'running', 'promote', true, $5, now(),
                     '[{\"label\": \"Pausing changes and copying the control plane\", \"status\": \"running\"}]')",
        )
        .bind(id)
        .bind(ws)
        .bind(source)
        .bind(target)
        .bind(phase)
        .execute(self.app.db())
        .await
        .unwrap();
        id
    }
}

fn server_of(dir: &std::path::Path) -> String {
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("node.json")).unwrap()).unwrap();
    meta["server"].as_str().unwrap().to_owned()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn maintenance_refuses_writes_then_the_old_dashboard_sends_people_on(pool: PgPool) {
    let s = setup(pool).await;
    let ws = s.app.create_workspace(&s.admin, "team").await;
    let (target, _) = s.node("new-main").await;
    let id = s.migration(None, target, "install").await;
    sqlx::query(
        "UPDATE server_migrations SET new_public_url = 'https://203.0.113.9' WHERE id = $1",
    )
    .bind(id)
    .execute(s.app.db())
    .await
    .unwrap();
    maintenance::sync(&s.app.state).await.unwrap();

    let m = s
        .app
        .call(Method::GET, "/api/maintenance", None, None)
        .await;
    assert_eq!(m.json["mode"], "maintenance");
    assert_eq!(m.json["newUrl"], "https://203.0.113.9");
    // Reads work, writes don't.
    let r = s
        .app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let w = s
        .app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(&s.admin),
            Some(json!({ "name": "shop" })),
        )
        .await;
    assert_eq!(w.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(w.code(), "maintenance");
    // Abort stays possible (and ends maintenance).
    let a = s
        .app
        .call(
            Method::POST,
            &format!("/api/server-migrations/{id}/abort"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{:?}", a.json);
    assert!(!s.app.state.maintenance.is_on());
    let w = s
        .app
        .call(
            Method::POST,
            &format!("/api/workspaces/{ws}/projects"),
            Some(&s.admin),
            Some(json!({ "name": "shop" })),
        )
        .await;
    assert_eq!(w.status, StatusCode::OK, "{:?}", w.json);

    // Retired: the API points elsewhere, pages redirect there.
    sqlx::query("UPDATE server_migrations SET retired_at = now(), new_node_url = 'https://203.0.113.9:7101' WHERE id = $1")
        .bind(id)
        .execute(s.app.db())
        .await
        .unwrap();
    maintenance::sync(&s.app.state).await.unwrap();
    let h = s.app.call(Method::GET, "/api/health", None, None).await;
    assert_eq!(h.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(h.code(), "moved");
    assert_eq!(h.json["error"]["movedTo"], "https://203.0.113.9");
    let page = s
        .app
        .send(
            Request::builder()
                .uri("/projects?x=1")
                .header(header::HOST, "198.51.100.1"),
            None,
            None,
        )
        .await;
    assert_eq!(page.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        page.headers[header::LOCATION],
        "https://203.0.113.9/projects?x=1"
    );
    // No job runs on a retired control plane.
    assert_eq!(s.app.state.maintenance.job_kinds(), Some(&[][..]));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_bundle_goes_once_and_only_to_the_new_server(pool: PgPool) {
    let s = setup(pool).await;
    let (target, target_dir) = s.node("new-main").await;
    let (other, other_dir) = s.node("worker").await;
    let id = s.migration(None, target, "install").await;
    let path = promote::bundle_path(&s.app.state, id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let bundle: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&path, &bundle).unwrap();
    let ticket = modula_crypto::token::generate(modula_crypto::token::prefix::PROMOTION).unwrap();
    sqlx::query("UPDATE server_migrations SET promote_ticket_hash = $2 WHERE id = $1")
        .bind(id)
        .bind(ticket.hash.as_bytes())
        .execute(s.app.db())
        .await
        .unwrap();
    use secrecy::ExposeSecret;
    let req = modula_proto::node::v1::PromoteRequest {
        request_id: id.to_string(),
        ticket: ticket.plaintext.expose_secret().to_owned(),
    };
    let other_id = modula_node::identity::Identity::load(&other_dir).unwrap();
    let dest = s.dir.join("fetched.tar");
    let err = modula_node::promote::fetch(&other_id, &req, &dest)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("refused"), "{err:#}");
    let _ = other;

    let target_id = modula_node::identity::Identity::load(&target_dir).unwrap();
    let n = modula_node::promote::fetch(&target_id, &req, &dest)
        .await
        .unwrap();
    assert_eq!(n, bundle.len() as u64);
    assert_eq!(std::fs::read(&dest).unwrap(), bundle);
    // Single use.
    let again = modula_node::promote::fetch(&target_id, &req, &dest)
        .await
        .unwrap_err();
    assert!(again.to_string().contains("refused"), "{again:#}");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_control_plane_hands_over(pool: PgPool) {
    let s = setup(pool).await;
    let (source, _) = s.node("old-main").await;
    let (target, target_dir) = s.node("new-main").await;
    let (worker, worker_dir) = s.node("worker").await;
    sqlx::query("UPDATE nodes SET public_ip = '127.0.0.1' WHERE id = $1")
        .bind(target)
        .execute(s.app.db())
        .await
        .unwrap();
    let id = s.migration(Some(source), target, "bundle").await;
    sqlx::query("UPDATE server_migrations SET new_node_url = $2 WHERE id = $1")
        .bind(id)
        .bind(format!("https://127.0.0.1:{}", s.port))
        .execute(s.app.db())
        .await
        .unwrap();
    maintenance::sync(&s.app.state).await.unwrap();
    assert!(s.app.state.maintenance.is_on());
    let before = fake_promoter().bundles.lock().unwrap().len();

    let st = s.app.state.clone();
    let mut done = false;
    for _ in 0..600 {
        if modula_server::migrate::server::poll(&st, id).await.unwrap() {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let row: (String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT status, error, steps::text FROM server_migrations WHERE id = $1")
            .bind(id)
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert!(done, "{row:?}");
    assert_eq!(row.0, "done", "{row:?}");

    // The new server got the bundle and installed it (fake).
    let bundles = fake_promoter().bundles.lock().unwrap().clone();
    assert_eq!(bundles.len(), before + 1);
    let mut names = Vec::new();
    let mut tar = tar::Archive::new(&bundles[before][..]);
    for e in tar.entries().unwrap() {
        let e = e.unwrap();
        names.push(e.path().unwrap().display().to_string());
    }
    for want in [
        "promotion.env",
        "control-plane.dump",
        "master.key",
        "uploads/ws/src.tar.gz",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
    assert!(
        !promote::bundle_path(&st, id).exists(),
        "the bundle is gone"
    );
    // Its node dials its own control plane; every other one the new address.
    let local = format!("https://127.0.0.1:{}", s.port);
    assert_eq!(server_of(&target_dir), local);
    wait_for("the worker switched", || {
        let d = worker_dir.clone();
        let l = local.clone();
        async move { server_of(&d) == l }
    })
    .await;
    let _ = worker;
    // This one retired.
    assert!(matches!(
        st.maintenance.get(),
        Some(maintenance::Mode::Moved { .. })
    ));

    // The copy on the new server finishes once its node is there.
    let id2 = Id::new();
    sqlx::query(
        "INSERT INTO server_migrations (id, workspace_id, source_node_id, source_name, target_location,
                                        target_server_type, target_node_id, status, step, main)
         SELECT $2, workspace_id, source_node_id, source_name, target_location, target_server_type,
                target_node_id, 'running', 'promote', true FROM server_migrations WHERE id = $1",
    )
    .bind(id)
    .bind(id2)
    .execute(s.app.db())
    .await
    .unwrap();
    promote::finalize(&st, id2).await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM nodes WHERE id = $1")
        .bind(source)
        .fetch_one(s.app.db())
        .await
        .unwrap();
    assert_eq!(status, "revoked");
    let m: String = sqlx::query_scalar("SELECT status FROM server_migrations WHERE id = $1")
        .bind(id2)
        .fetch_one(s.app.db())
        .await
        .unwrap();
    assert_eq!(m, "done");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_node_refuses_an_address_that_does_not_work(pool: PgPool) {
    let s = setup(pool).await;
    let (_, dir) = s.node("worker").await;
    let before = server_of(&dir);
    let err = modula_node::address::switch(&dir, "https://127.0.0.1:1", Duration::from_millis(100))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot reach"), "{err:#}");
    assert_eq!(server_of(&dir), before);
    let changed = modula_node::address::switch(
        &dir,
        &format!("https://127.0.0.1:{}", s.port),
        Duration::from_secs(5),
    )
    .await
    .unwrap();
    assert!(changed);
    assert_eq!(server_of(&dir), format!("https://127.0.0.1:{}", s.port));
}
