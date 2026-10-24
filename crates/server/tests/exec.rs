//! Shells in containers end to end: the CLI's exec client → WebSocket on the
//! real HTTP server → the node's gRPC stream (mTLS) → an interactive command
//! in a fake runtime (which echoes stdin; `exit <n>` exits). Also the
//! `run-env` endpoint behind `modula run`.

mod common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use common::*;
use modula_cli::api::api_error;
use modula_cli::config::Login;
use modula_cli::shell::{Event, Exec, ExecOptions};
use modula_id::Id;
use modula_node::join::{JoinArgs, join};
use modula_node::runtime::fake::FakeRuntime;
use serde_json::json;
use sqlx::PgPool;
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

struct Setup {
    app: TestApp,
    admin: String,
    api_url: String,
    token: String,
    rt: Arc<FakeRuntime>,
    _stop: CancellationToken,
}

impl Drop for Setup {
    fn drop(&mut self) {
        self._stop.cancel();
    }
}

async fn setup(pool: PgPool) -> Setup {
    let app = TestApp::new(pool);
    let admin = app.setup_admin().await;
    app.state.execs.set_check_every(Duration::from_millis(100));
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
    let dir = std::env::temp_dir().join(format!("modula-exec-test-{}", Id::new()));
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
    let caps = modula_node::reconcile::Capabilities {
        edge: None,
        build: None,
        db: None,
        upgrade: None,
        mesh: None,
        access_log: None,
    };
    tokio::spawn(modula_node::session::run(
        dir,
        rt.clone(),
        caps,
        stop.child_token(),
    ));
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
        rt,
        _stop: stop,
    }
}

impl Setup {
    /// A service; returns (workspace, instance, deployment once active).
    async fn service(&self, deploy: bool) -> (Id, Id, Option<Id>) {
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
                    "name": "api",
                    "config": { "source": { "type": "image", "image": "api:1" }, "port": 8080 },
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
            return (ws, instance, None);
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
        (ws, instance, Some(deployment))
    }

    fn login(&self, token: &str) -> Login {
        Login {
            url: self.api_url.parse().unwrap(),
            token: token.to_owned(),
            access: None,
        }
    }

    async fn exec(&self, deployment: Id, opts: ExecOptions) -> anyhow::Result<Exec> {
        Exec::open(&self.login(&self.token), &deployment.to_string(), &opts).await
    }

    async fn wait_closed(&self) {
        let st = self.app.state.clone();
        wait_for("execs closed", || {
            let st = st.clone();
            async move { st.execs.is_empty() }
        })
        .await;
    }

    async fn audit(&self, action: &str) -> Vec<serde_json::Value> {
        sqlx::query_scalar("SELECT metadata FROM audit_log WHERE action = $1 ORDER BY id")
            .bind(action)
            .fetch_all(self.app.db())
            .await
            .unwrap()
    }
}

async fn next(exec: &mut Exec) -> Event {
    tokio::time::timeout(Duration::from_secs(10), exec.next())
        .await
        .expect("output in time")
}

/// Stdout until `want` bytes arrived.
async fn read_stdout(exec: &mut Exec, want: usize) -> Vec<u8> {
    let mut got = Vec::new();
    while got.len() < want {
        match next(exec).await {
            Event::Stdout(b) => got.extend_from_slice(&b),
            other => panic!("unexpected {other:?}"),
        }
    }
    got
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_shell_round_trips_and_reports_its_exit_code(pool: PgPool) {
    let s = setup(pool).await;
    let (_, _, deployment) = s.service(true).await;
    let deployment = deployment.unwrap();

    let opts = ExecOptions {
        tty: true,
        cols: 100,
        rows: 30,
        ..Default::default()
    };
    let mut exec = s.exec(deployment, opts).await.expect("exec opens");
    assert_eq!(s.app.state.execs.len(), 1);
    {
        let sessions = s.rt.sessions.lock().unwrap();
        assert_eq!(sessions[0].spec.cmd, modula_node::exec::DEFAULT_SHELL);
        assert!(sessions[0].spec.tty);
        assert_eq!((sessions[0].spec.cols, sessions[0].spec.rows), (100, 30));
        assert_eq!(sessions[0].deployment_id, deployment.to_string());
    }

    exec.stdin(b"echo hi\n").await.unwrap();
    assert_eq!(read_stdout(&mut exec, 8).await, b"echo hi\n");
    exec.resize(120, 40).await.unwrap();
    let rt = s.rt.clone();
    wait_for("resize", || {
        let rt = rt.clone();
        async move { rt.resizes.lock().unwrap().contains(&(120, 40)) }
    })
    .await;

    // Several windows each way, so credits must flow.
    for _ in 0..10 {
        exec.stdin(&[b'a'; 100_000]).await.unwrap();
        assert_eq!(read_stdout(&mut exec, 100_000).await.len(), 100_000);
    }

    exec.stdin(b"\nexit 7\n").await.unwrap();
    let _ = read_stdout(&mut exec, 8).await;
    match next(&mut exec).await {
        Event::Exit { code, .. } => assert_eq!(code, Some(7)),
        other => panic!("unexpected {other:?}"),
    }
    s.wait_closed().await;

    // Audited without what was typed.
    let opened = s.audit("exec.open").await;
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0]["replica"], 0);
    assert!(opened[0]["command"].is_null());
    wait_for("close audited", || async {
        s.audit("exec.close").await.len() == 1
    })
    .await;
    let closed = s.audit("exec.close").await.pop().unwrap();
    assert_eq!(closed["exitCode"], 7);
    assert!(closed["bytesIn"].as_u64().unwrap() > 1_000_000);
    let logged: String =
        sqlx::query_scalar("SELECT string_agg(metadata::text, ' ') FROM audit_log")
            .fetch_one(s.app.db())
            .await
            .unwrap();
    assert!(!logged.contains("echo hi"));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn commands_get_stderr_eof_and_close_with_their_deployment(pool: PgPool) {
    let s = setup(pool).await;
    let (_, _, deployment) = s.service(true).await;
    let deployment = deployment.unwrap();

    // A command without a terminal: stderr apart, stdin can end.
    let opts = ExecOptions {
        cmd: vec!["cat".into(), "--token=hunter2".into()],
        ..Default::default()
    };
    let mut exec = s.exec(deployment, opts).await.unwrap();
    assert_eq!(
        s.rt.sessions.lock().unwrap()[0].spec.cmd,
        ["cat", "--token=hunter2"]
    );
    assert!(!s.rt.sessions.lock().unwrap()[0].spec.tty);
    exec.stdin(b"stderr oops\n").await.unwrap();
    assert_eq!(read_stdout(&mut exec, 12).await, b"stderr oops\n");
    assert_eq!(next(&mut exec).await, Event::Stderr(b"oops\n".to_vec()));
    exec.eof().await.unwrap();
    match next(&mut exec).await {
        Event::Exit { code, .. } => assert_eq!(code, Some(0)),
        other => panic!("unexpected {other:?}"),
    }
    s.wait_closed().await;
    let opened = s.audit("exec.open").await;
    assert_eq!(opened[0]["command"], "cat");
    assert!(!opened[0].to_string().contains("hunter2"));

    // The client hangs up: the exec closes everywhere.
    let exec = s.exec(deployment, ExecOptions::default()).await.unwrap();
    drop(exec);
    s.wait_closed().await;

    // Removing the deployment closes open shells.
    let mut exec = s.exec(deployment, ExecOptions::default()).await.unwrap();
    let r = s
        .app
        .call(
            Method::DELETE,
            &format!("/api/deployments/{deployment}"),
            Some(&s.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    match next(&mut exec).await {
        Event::Exit { code, reason } => {
            assert_eq!(code, None);
            assert_eq!(reason, "the deployment was removed");
        }
        other => panic!("unexpected {other:?}"),
    }
    s.wait_closed().await;

    // And it can't be opened again.
    let err = s
        .exec(deployment, ExecOptions::default())
        .await
        .err()
        .unwrap();
    let api = api_error(&err).expect("an API error");
    assert_eq!(api.status, 409, "{}", api.message);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn shells_need_member_sudo_for_sessions_and_our_origin(pool: PgPool) {
    let s = setup(pool).await;
    let (ws, _, deployment) = s.service(true).await;
    let deployment = deployment.unwrap();

    // Bad commands are refused before reaching the node.
    let url = format!(
        "{}api/deployments/{deployment}/exec?cmd=ls",
        s.api_url.replace("http://", "ws://")
    );
    let status = ws_status(&url, None, Some(&s.token)).await;
    assert_eq!(status, 400);

    // A deployer (and so a viewer) can't open shells.
    let (deployer_id, deployer) = s.app.user("deployer@example.com").await;
    s.app.add_member(ws, deployer_id, "deployer").await;
    let r = s
        .app
        .call(
            Method::POST,
            "/api/account/tokens",
            Some(&deployer),
            Some(json!({ "name": "cli" })),
        )
        .await;
    let token = r.json["token"].as_str().unwrap().to_owned();
    let err = Exec::open(
        &s.login(&token),
        &deployment.to_string(),
        &ExecOptions::default(),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(api_error(&err).unwrap().status, 403);

    let url = format!(
        "{}api/deployments/{deployment}/exec",
        s.api_url.replace("http://", "ws://")
    );
    let cookie = format!("{COOKIE}={}", s.admin);
    // A cookie session must come from our own origin.
    assert_eq!(ws_status(&url, None, None).await, 401);
    assert_eq!(ws_cookie_status(&url, &cookie, None).await, 403);
    assert_eq!(
        ws_cookie_status(&url, &cookie, Some("https://evil.test")).await,
        403
    );
    assert_eq!(ws_cookie_status(&url, &cookie, Some(ORIGIN)).await, 101);
    s.wait_closed().await;
    // And be recently re-authenticated (API tokens never are, and pass).
    s.app.drop_sudo().await;
    assert_eq!(ws_cookie_status(&url, &cookie, Some(ORIGIN)).await, 403);
    let exec = s.exec(deployment, ExecOptions::default()).await.unwrap();
    drop(exec);
    s.wait_closed().await;
}

async fn ws_status(url: &str, cookie: Option<&str>, token: Option<&str>) -> u16 {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().unwrap();
    if let Some(c) = cookie {
        req.headers_mut().insert("cookie", c.parse().unwrap());
    }
    if let Some(t) = token {
        req.headers_mut()
            .insert("authorization", format!("Bearer {t}").parse().unwrap());
    }
    connect_status(req).await
}

async fn ws_cookie_status(url: &str, cookie: &str, origin: Option<&str>) -> u16 {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert("cookie", cookie.parse().unwrap());
    if let Some(o) = origin {
        req.headers_mut().insert("origin", o.parse().unwrap());
    }
    connect_status(req).await
}

async fn connect_status(req: tokio_tungstenite::tungstenite::handshake::client::Request) -> u16 {
    let tcp = TcpStream::connect(req.uri().authority().unwrap().as_str())
        .await
        .unwrap();
    match tokio_tungstenite::client_async(req, tcp).await {
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(res)) => res.status().as_u16(),
        Err(e) => panic!("{e}"),
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn run_env_withholds_sealed_values_and_is_audited(pool: PgPool) {
    let s = setup(pool).await;
    let (ws, instance, _) = s.service(false).await;
    let environment: Id =
        sqlx::query_scalar("SELECT environment_id FROM service_instances WHERE id = $1")
            .bind(instance)
            .fetch_one(s.app.db())
            .await
            .unwrap();
    let r = s
        .app
        .call(
            Method::PATCH,
            &format!("/api/environments/{environment}/variables"),
            Some(&s.admin),
            Some(json!({ "set": { "SIGNING_KEY": "k3y", "REGION": "eu" }, "sealed": ["SIGNING_KEY"] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = s
        .app
        .call(
            Method::PATCH,
            &format!("/api/service-instances/{instance}/variables"),
            Some(&s.admin),
            Some(json!({
                "set": {
                    "DATABASE_URL": "postgres://u:p@db/${{shared.REGION}}",
                    "API_SECRET": "hush",
                    "DERIVED": "x-${{API_SECRET}}",
                    "SIGNED": "${{shared.SIGNING_KEY}}",
                    "GHOST": "${{nope.X}}",
                },
                "sealed": ["API_SECRET"],
            })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    let path = format!("/api/service-instances/{instance}/run-env");
    let r = s.app.call(Method::GET, &path, Some(&s.admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let vars = &r.json["variables"];
    assert_eq!(vars["DATABASE_URL"], "postgres://u:p@db/eu");
    assert_eq!(vars["GHOST"], "");
    assert_eq!(vars["MODULA_SERVICE_NAME"], "api");
    assert!(vars.get("PORT").is_none());
    for hidden in ["API_SECRET", "DERIVED", "SIGNED", "SIGNING_KEY", "REGION"] {
        assert!(vars.get(hidden).is_none(), "{hidden} leaked");
    }
    let mut withheld: Vec<&str> = r.json["withheld"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    withheld.sort();
    assert_eq!(withheld, ["API_SECRET", "DERIVED", "SIGNED"]);
    assert_eq!(r.json["missing"], json!(["nope.X"]));
    let body = r.json.to_string();
    assert!(!body.contains("hush") && !body.contains("k3y"));

    // Audited with the names, never the values.
    let audit = s.audit("variable.run_env").await;
    assert_eq!(audit.len(), 1);
    assert!(
        audit[0]["names"]
            .as_array()
            .unwrap()
            .contains(&json!("DATABASE_URL"))
    );
    assert!(!audit[0].to_string().contains("postgres://"));

    // The CLI's API token gets it without sudo; a cookie session needs it.
    let api = modula_cli::api::Api::new(s.login(&s.token)).unwrap();
    let env = api.run_env(&instance.to_string()).await.unwrap();
    assert_eq!(env.variables["DATABASE_URL"], "postgres://u:p@db/eu");
    s.app.drop_sudo().await;
    let r = s.app.call(Method::GET, &path, Some(&s.admin), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.code(), "sudo_required");

    // Deployers can't read values.
    let (deployer_id, deployer) = s.app.user("deployer@example.com").await;
    s.app.add_member(ws, deployer_id, "deployer").await;
    let r = s.app.call(Method::GET, &path, Some(&deployer), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}
