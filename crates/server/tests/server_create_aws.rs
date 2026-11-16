//! `modula server create --provider aws` end to end, without touching AWS:
//! the CLI's flow against the EC2 mock, a fake installer progress endpoint,
//! a mock Cloudflare, and this crate's real server as the new instance.
#![allow(dead_code)]

mod common;

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use common::*;
use futures::future::BoxFuture;
use modula_cli::cli::{Global, Provider, ServerCreateArgs};
use modula_cli::context::{Ctx, Session};
use modula_cli::output::{Output, UsageError};
use modula_cli::provision::cloud::AwsKeys;
use modula_cli::provision::flow::Settings;
use modula_cli::provision::{EnvSecrets, create_with, dns::Resolver};
use modula_id::Id;
use serde_json::{Value, json};
use sqlx::PgPool;

include!("common/ec2_mock.rs");

const CLOUDFLARE: &str = "cf-test-token-0123456789";

#[derive(Default)]
struct Side {
    reports: Vec<Value>,
    dns_records: Vec<Value>,
}

type SharedSide = Arc<Mutex<Side>>;

fn quoted_arg(script: &str, flag: &str) -> Option<String> {
    let rest = script.split(&format!("{flag} '")).nth(1)?;
    Some(rest.split('\'').next()?.to_owned())
}

/// Cloudflare and the installer's progress file.
async fn side_services() -> (String, SharedSide) {
    let m = SharedSide::default();
    let app = Router::new()
        .route(
            "/cf/user/tokens/verify",
            get(|| async {
                Json(json!({"success": true, "errors": [], "result": {"id": "t", "status": "active"}}))
            }),
        )
        .route(
            "/cf/zones",
            get(|| async {
                Json(json!({"success": true, "errors": [], "result": [{"id": "zone1", "name": "cloud.test"}]}))
            }),
        )
        .route(
            "/cf/zones/zone1/dns_records",
            get(|State(m): State<SharedSide>| async move {
                let records = m.lock().unwrap().dns_records.clone();
                Json(json!({"success": true, "errors": [], "result": records}))
            })
            .post(|State(m): State<SharedSide>, Json(body): Json<Value>| async move {
                let mut r = body.clone();
                r["id"] = json!("rec1");
                m.lock().unwrap().dns_records.push(r.clone());
                Json(json!({"success": true, "errors": [], "result": r}))
            }),
        )
        .route(
            "/cf/zones/zone1/dns_records/{id}",
            delete(|State(m): State<SharedSide>, Path(id): Path<String>| async move {
                m.lock().unwrap().dns_records.retain(|r| r["id"] != id.as_str());
                Json(json!({"success": true, "errors": [], "result": {"id": id}}))
            }),
        )
        .route(
            "/progress/.modula-progress/{file}",
            get(|State(m): State<SharedSide>| async move {
                let mut c = m.lock().unwrap();
                if c.reports.is_empty() {
                    return (StatusCode::NOT_FOUND, Json(Value::Null));
                }
                (StatusCode::OK, Json(c.reports.remove(0)))
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, m)
}

struct FakeDns;

impl Resolver for FakeDns {
    fn ipv4<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Result<Vec<Ipv4Addr>, String>> {
        Box::pin(async { Ok(vec![EIP.parse().unwrap()]) })
    }
}

/// The mock, set up like install.sh would use the user data: instances run
/// when looked at, and the setup token is preseeded on the new instance.
async fn aws(db: PgPool) -> (url::Url, Shared) {
    let (url, mock) = mock_aws().await;
    let mut m = mock.lock().unwrap();
    m.auto_run = true;
    m.on_run = Some(Box::new(move |script: &str| {
        if let Some(token) = quoted_arg(script, "--setup-token") {
            let hash = modula_crypto::token::hash(&token);
            let db = db.clone();
            tokio::spawn(async move {
                modula_server::auth::setup::preseed_if_pending(&db, hash)
                    .await
                    .unwrap();
            });
        }
    }));
    drop(m);
    (url, mock)
}

async fn instance(pool: PgPool, aws: &url::Url, side: &str) -> String {
    let mut cfg = common::config();
    cfg.aws_api = Some(aws.clone());
    cfg.cloudflare_api = format!("{side}/cf").parse().unwrap();
    let app = TestApp::with_config(pool, cfg);
    let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", http.local_addr().unwrap());
    let router = app.router.clone();
    tokio::spawn(async move {
        axum::serve(
            http,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    url
}

fn settings(aws: &url::Url, side: &str, instance: &str) -> Settings {
    Settings {
        hetzner_api: "http://127.0.0.1:9/v1".parse().unwrap(),
        aws_api: Some(aws.clone()),
        cloudflare_api: format!("{side}/cf").parse().unwrap(),
        instance_via: Some(instance.parse().unwrap()),
        progress_via: Some(format!("{side}/progress/").parse().unwrap()),
        dns: Box::new(FakeDns),
        poll: Duration::from_millis(20),
        dns_timeout: Duration::from_secs(20),
        install_timeout: Duration::from_secs(20),
    }
}

fn ctx(dir: &std::path::Path) -> (Ctx, Arc<Mutex<Vec<u8>>>) {
    let session = Session {
        config_path: dir.join("config.toml"),
        cwd: dir.to_path_buf(),
        env_url: None,
        env_token: None,
        interactive: false,
    };
    let global = Global {
        json: true,
        yes: true,
        ..Default::default()
    };
    let (out, buf) = Output::capture(true);
    (Ctx::new(session, global, out).unwrap(), buf)
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("modula-server-create-aws-{}", Id::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn args(dir: &std::path::Path) -> ServerCreateArgs {
    let key = dir.join("id_ed25519.pub");
    std::fs::write(&key, "ssh-ed25519 AAAAnew ada@laptop\n").unwrap();
    ServerCreateArgs {
        provider: Some(Provider::Aws),
        location: Some("us-east-1".into()),
        server_type: Some("t3.large".into()),
        name: Some("modula-main".into()),
        domain: Some("cloud.test".into()),
        email: Some("ada@example.com".into()),
        ssh_key_file: Some(key),
        admin_name: Some("Ada".into()),
        ..Default::default()
    }
}

fn env(cloudflare: bool) -> EnvSecrets {
    EnvSecrets {
        hcloud: None,
        aws: Some(AwsKeys {
            access_key_id: KEY_ID.into(),
            secret_access_key: SECRET.to_owned().into(),
        }),
        cloudflare: cloudflare.then(|| CLOUDFLARE.to_owned().into()),
        admin_password: Some(PASSWORD.to_owned().into()),
    }
}

fn events(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
    String::from_utf8(buf.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn report(state: &str) -> Value {
    json!({"state": state, "steps": [{"name": "Checking this server", "state": "done", "at": 0}]})
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn creates_a_main_server_on_ec2(pool: PgPool) {
    let (aws_url, mock) = aws(pool.clone()).await;
    let (side, s) = side_services().await;
    s.lock().unwrap().reports = vec![report("running"), report("handoff")];
    let url = instance(pool.clone(), &aws_url, &side).await;
    let dir = temp_dir();
    let (mut ctx, buf) = ctx(&dir);
    let mut a = args(&dir);
    a.cloudflare_dns = true;
    let code = create_with(&mut ctx, a, env(true), settings(&aws_url, &side, &url))
        .await
        .unwrap();
    let ev = events(&buf);
    assert_eq!(code, 0, "{ev:#?}");

    let plan = &ev[0];
    assert_eq!(plan["event"], "plan");
    assert_eq!(plan["provider"], "aws");
    assert!(plan["monthly"].as_str().unwrap().starts_with('$'));
    let created = ev.iter().find(|e| e["event"] == "server_created").unwrap();
    assert_eq!(created["id"], "us-east-1/i-0");
    assert_eq!(created["ip"], EIP);
    assert!(ev.iter().any(|e| e["event"] == "server_running"));

    // The address came from polling, and Cloudflare got it.
    assert_eq!(s.lock().unwrap().dns_records[0]["content"], EIP);

    // Both credentials became integrations on the instance.
    let kinds: Vec<String> = sqlx::query_scalar("SELECT kind FROM integrations ORDER BY kind")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kinds, ["aws", "cloudflare"]);

    let m = mock.lock().unwrap();
    let inst = &m.instances[0];
    assert_eq!(inst.instance_type, "t3.large");
    assert_eq!(inst.tags["modula"], "1");
    assert_eq!(inst.tags["role"], "main");
    assert!(inst.tags.contains_key("modula-id"));
    assert_eq!(inst.key.as_deref(), Some("ada@laptop"));
    assert_eq!(m.addresses[0].instance.as_deref(), Some("i-0"));
    // The EIP wait precedes the install; no credential is in the user data.
    let wait = inst.user_data.find(&format!("eip='{EIP}'")).unwrap();
    assert!(wait < inst.user_data.find("--setup-token").unwrap());
    for secret in [SECRET, KEY_ID, CLOUDFLARE, PASSWORD] {
        assert!(!inst.user_data.contains(secret));
    }
    // It carries the main groups, which open the node port.
    let main: Vec<&Group> = m
        .groups
        .iter()
        .filter(|g| g.tags.get("role").map(String::as_str) == Some("main"))
        .collect();
    assert!(!main.is_empty());
    assert!(main.iter().all(|g| inst.groups.contains(&g.id)));
    assert!(
        main.iter()
            .any(|g| g.rules.iter().any(|r| r.1 == 7101 && r.2 == 7101))
    );
    drop(m);

    let saved = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(saved.contains("url = \"https://cloud.test/\""), "{saved}");
    assert!(!saved.contains(SECRET));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn an_install_failure_terminates_and_releases(pool: PgPool) {
    let (aws_url, mock) = aws(pool.clone()).await;
    let (side, s) = side_services().await;
    let mut failed = report("failed");
    failed["error"] = json!({"step": "Setting up Postgres", "message": "no", "hint": "h"});
    s.lock().unwrap().reports = vec![failed];
    let url = instance(pool, &aws_url, &side).await;
    let dir = temp_dir();
    let (mut ctx, buf) = ctx(&dir);
    let a = args(&dir);
    let code = create_with(&mut ctx, a, env(false), settings(&aws_url, &side, &url))
        .await
        .unwrap();
    let ev = events(&buf);
    assert_eq!(code, 1, "{ev:#?}");
    let cleanup = ev.iter().find(|e| e["event"] == "cleanup").unwrap();
    assert_eq!(cleanup["serverDeleted"], true);
    assert_eq!(ev.last().unwrap()["serverKept"], false);
    let m = mock.lock().unwrap();
    assert_eq!(m.instances[0].state, "shutting-down");
    assert!(m.addresses.is_empty(), "the Elastic IP is released");
    drop(m);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_failed_launch_leaves_no_address(pool: PgPool) {
    let (aws_url, mock) = aws(pool.clone()).await;
    mock.lock().unwrap().fail_run = true;
    let (side, _s) = side_services().await;
    let url = instance(pool, &aws_url, &side).await;
    let dir = temp_dir();
    let (mut ctx, buf) = ctx(&dir);
    let a = args(&dir);
    let code = create_with(&mut ctx, a, env(false), settings(&aws_url, &side, &url))
        .await
        .unwrap();
    let ev = events(&buf);
    assert_eq!(code, 1, "{ev:#?}");
    assert!(
        ev.last().unwrap()["error"]
            .as_str()
            .unwrap()
            .contains("capacity in any zone")
    );
    assert!(mock.lock().unwrap().addresses.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn missing_aws_keys_are_named_before_any_call(pool: PgPool) {
    let (aws_url, mock) = aws(pool.clone()).await;
    let (side, _s) = side_services().await;
    let url = instance(pool, &aws_url, &side).await;
    let dir = temp_dir();
    let (mut ctx, _) = ctx(&dir);
    let mut e = env(false);
    e.aws = None;
    let err = create_with(&mut ctx, args(&dir), e, settings(&aws_url, &side, &url))
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<UsageError>().is_some());
    assert!(err.to_string().contains("AWS_SECRET_ACCESS_KEY"), "{err}");
    assert!(mock.lock().unwrap().calls.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}
