//! `modula server create` end to end, without touching a real cloud: the
//! CLI's flow against a mock Hetzner API, a mock Cloudflare API, a fake
//! installer progress endpoint, and this crate's real HTTP server standing
//! in for the new instance (its setup token preseeded from the user data the
//! CLI sent, as install.sh would).

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use common::*;
use futures::future::BoxFuture;
use modula_cli::cli::{Global, OnFailure, ServerCreateArgs};
use modula_cli::context::{Ctx, Session};
use modula_cli::output::{Output, UsageError};
use modula_cli::provision::flow::Settings;
use modula_cli::provision::{EnvSecrets, create_with, dns::Resolver};
use modula_id::Id;
use serde_json::{Value, json};
use sqlx::PgPool;

const HCLOUD: &str = "hcloud-test-token-0123456789abcdefghijklmnopqrstuvwxyz";
const CLOUDFLARE: &str = "cf-test-token-0123456789";
const IP: &str = "127.0.0.1";

#[derive(Default)]
struct Cloud {
    calls: Vec<String>,
    firewalls: Vec<Value>,
    servers: Vec<Value>,
    deleted_servers: Vec<i64>,
    deleted_firewalls: Vec<i64>,
    ssh_keys: Vec<Value>,
    dns_records: Vec<Value>,
    /// What the fake installer reports, in turn (then 404: handed off).
    reports: Vec<Value>,
    /// The new instance's DB, to preseed the setup token like install.sh.
    db: Option<PgPool>,
}

type Shared = Arc<Mutex<Cloud>>;

fn page(key: &str, items: Value) -> Value {
    json!({ key: items, "meta": { "pagination": { "page": 1, "next_page": null } } })
}

fn authorized(h: &HeaderMap, token: &str) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {token}"))
}

macro_rules! hetzner {
    ($m:expr, $h:expr, $what:expr) => {{
        $m.lock().unwrap().calls.push($what.to_string());
        if !authorized(&$h, HCLOUD) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": {"code": "unauthorized", "message": "unable to authenticate"}})),
            );
        }
    }};
}

/// The value after `flag` in a shell line of single-quoted words.
fn quoted_arg(script: &str, flag: &str) -> Option<String> {
    let rest = script.split(&format!("{flag} '")).nth(1)?;
    Some(rest.split('\'').next()?.to_owned())
}

async fn mock_cloud(db: PgPool) -> (String, Shared) {
    let m: Shared = Arc::new(Mutex::new(Cloud {
        db: Some(db),
        ssh_keys: vec![
            json!({"id": 42, "name": "laptop", "fingerprint": "b7:2f", "public_key": "ssh-ed25519 AAAAexisting laptop"}),
        ],
        ..Default::default()
    }));
    let price = |loc: &str, gross: &str| json!({"location": loc, "price_hourly": {"net": "0", "gross": "0"}, "price_monthly": {"net": "1", "gross": gross}});
    let types = json!([
        {"id": 1, "name": "cx23", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "architecture": "x86", "prices": [price("fsn1", "4.5101")], "locations": [{"name": "fsn1", "available": true, "recommended": true}]},
        {"id": 2, "name": "cx33", "cores": 4, "memory": 8.0, "disk": 80, "cpu_type": "shared", "architecture": "x86", "prices": [price("fsn1", "8.2100")], "locations": [{"name": "fsn1", "available": true}]}
    ]);
    let app = Router::new()
        // Hetzner
        .route("/hetzner/v1/locations", get(|State(m): State<Shared>, h: HeaderMap| async move {
            hetzner!(m, h, "GET /locations");
            (StatusCode::OK, Json(page("locations", json!([
                {"name": "fsn1", "city": "Falkenstein", "country": "DE", "network_zone": "eu-central"},
                {"name": "hel1", "city": "Helsinki", "country": "FI", "network_zone": "eu-central"}
            ]))))
        }))
        .route("/hetzner/v1/server_types", get(move |State(m): State<Shared>, h: HeaderMap| {
            let types = types.clone();
            async move {
                hetzner!(m, h, "GET /server_types");
                (StatusCode::OK, Json(page("server_types", types)))
            }
        }))
        .route("/hetzner/v1/ssh_keys", get(|State(m): State<Shared>, h: HeaderMap| async move {
            hetzner!(m, h, "GET /ssh_keys");
            let keys = m.lock().unwrap().ssh_keys.clone();
            (StatusCode::OK, Json(page("ssh_keys", json!(keys))))
        }).post(|State(m): State<Shared>, h: HeaderMap, Json(body): Json<Value>| async move {
            hetzner!(m, h, "POST /ssh_keys");
            let mut c = m.lock().unwrap();
            let mut key = body.clone();
            key["id"] = json!(43);
            c.ssh_keys.push(key.clone());
            (StatusCode::CREATED, Json(json!({"ssh_key": key})))
        }))
        .route("/hetzner/v1/firewalls", get(|State(m): State<Shared>, h: HeaderMap| async move {
            hetzner!(m, h, "GET /firewalls");
            let c = m.lock().unwrap();
            let live: Vec<Value> = c.firewalls.iter().filter(|f| !c.deleted_firewalls.contains(&f["id"].as_i64().unwrap())).cloned().collect();
            (StatusCode::OK, Json(page("firewalls", json!(live))))
        }).post(|State(m): State<Shared>, h: HeaderMap, Json(body): Json<Value>| async move {
            hetzner!(m, h, "POST /firewalls");
            let mut c = m.lock().unwrap();
            let mut fw = body.clone();
            fw["id"] = json!(100 + c.firewalls.len() as i64);
            c.firewalls.push(fw.clone());
            (StatusCode::CREATED, Json(json!({"firewall": fw, "actions": []})))
        }))
        .route("/hetzner/v1/firewalls/{id}", delete(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
            hetzner!(m, h, format!("DELETE /firewalls/{id}"));
            m.lock().unwrap().deleted_firewalls.push(id);
            (StatusCode::NO_CONTENT, Json(Value::Null))
        }))
        .route("/hetzner/v1/servers", post(|State(m): State<Shared>, h: HeaderMap, Json(body): Json<Value>| async move {
            hetzner!(m, h, "POST /servers");
            let mut c = m.lock().unwrap();
            let id = 5000 + c.servers.len() as i64;
            let mut stored = body.clone();
            stored["id"] = json!(id);
            c.servers.push(stored);
            // What install.sh does with --setup-token: only the hash is stored.
            let user_data = body["user_data"].as_str().unwrap_or_default();
            if let (Some(token), Some(db)) = (quoted_arg(user_data, "--setup-token"), c.db.clone()) {
                let hash = modula_crypto::token::hash(&token);
                tokio::spawn(async move {
                    modula_server::auth::setup::preseed_if_pending(&db, hash).await.unwrap();
                });
            }
            (StatusCode::CREATED, Json(json!({
                "server": {"id": id, "name": body["name"], "status": "initializing", "public_net": {"ipv4": {"ip": IP}}},
                "action": {"id": 900, "status": "running", "error": null},
                "root_password": null
            })))
        }))
        .route("/hetzner/v1/servers/{id}", delete(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
            hetzner!(m, h, format!("DELETE /servers/{id}"));
            m.lock().unwrap().deleted_servers.push(id);
            (StatusCode::OK, Json(json!({"action": {"id": 901, "status": "running"}})))
        }))
        .route("/hetzner/v1/actions/{id}", get(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
            hetzner!(m, h, format!("GET /actions/{id}"));
            (StatusCode::OK, Json(json!({"action": {"id": id, "status": "success", "error": null}})))
        }))
        // Cloudflare
        .route("/cf/user/tokens/verify", get(|h: HeaderMap| async move {
            assert!(authorized(&h, CLOUDFLARE));
            Json(json!({"success": true, "errors": [], "result": {"id": "t", "status": "active"}}))
        }))
        .route("/cf/zones", get(|h: HeaderMap, axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
            assert!(authorized(&h, CLOUDFLARE));
            let zones = if q.get("name").map(String::as_str) == Some("cloud.test") {
                json!([{"id": "zone1", "name": "cloud.test"}])
            } else {
                json!([])
            };
            Json(json!({"success": true, "errors": [], "result": zones}))
        }))
        .route("/cf/zones/zone1/dns_records", get(|State(m): State<Shared>| async move {
            let records = m.lock().unwrap().dns_records.clone();
            Json(json!({"success": true, "errors": [], "result": records}))
        }).post(|State(m): State<Shared>, Json(body): Json<Value>| async move {
            let mut c = m.lock().unwrap();
            c.calls.push(format!("CF create {} {} proxied={}", body["type"], body["content"], body["proxied"]));
            let mut r = body.clone();
            r["id"] = json!("rec1");
            c.dns_records.push(r.clone());
            Json(json!({"success": true, "errors": [], "result": r}))
        }))
        .route("/cf/zones/zone1/dns_records/{id}", delete(|State(m): State<Shared>, Path(id): Path<String>| async move {
            let mut c = m.lock().unwrap();
            c.calls.push(format!("CF delete {id}"));
            c.dns_records.retain(|r| r["id"] != id.as_str());
            Json(json!({"success": true, "errors": [], "result": {"id": id}}))
        }))
        // The installer's progress file (any token; each GET is the next report).
        .route("/progress/.modula-progress/{file}", get(|State(m): State<Shared>, Path(file): Path<String>| async move {
            assert!(file.ends_with(".json") && file.len() == 48, "{file}");
            let mut c = m.lock().unwrap();
            if c.reports.is_empty() {
                return (StatusCode::NOT_FOUND, Json(Value::Null));
            }
            (StatusCode::OK, Json(c.reports.remove(0)))
        }))
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, m)
}

/// Public DNS: the domain points at the server once Cloudflare has the record
/// (or right away without Cloudflare).
struct FakeDns(Shared, bool);

impl Resolver for FakeDns {
    fn ipv4<'a>(&'a self, _name: &'a str) -> BoxFuture<'a, Result<Vec<Ipv4Addr>, String>> {
        Box::pin(async move {
            let c = self.0.lock().unwrap();
            let pointed = !self.1 || c.dns_records.iter().any(|r| r["content"] == IP);
            Ok(if pointed {
                vec![IP.parse().unwrap()]
            } else {
                vec![]
            })
        })
    }
}

/// The real server as the new instance (public URL https://cloud.test).
async fn instance(pool: PgPool) -> String {
    let app = TestApp::new(pool);
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

fn settings(cloud: &str, m: &Shared, instance: &str, cloudflare: bool) -> Settings {
    Settings {
        hetzner_api: format!("{cloud}/hetzner/v1").parse().unwrap(),
        cloudflare_api: format!("{cloud}/cf").parse().unwrap(),
        instance_via: Some(instance.parse().unwrap()),
        progress_via: Some(format!("{cloud}/progress/").parse().unwrap()),
        dns: Box::new(FakeDns(m.clone(), cloudflare)),
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
    let dir = std::env::temp_dir().join(format!("modula-server-create-{}", Id::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn args() -> ServerCreateArgs {
    ServerCreateArgs {
        location: Some("fsn1".into()),
        server_type: Some("cx33".into()),
        name: Some("modula-main".into()),
        domain: Some("cloud.test".into()),
        email: Some("ada@example.com".into()),
        ssh_key_id: Some(42),
        admin_name: Some("Ada".into()),
        ..Default::default()
    }
}

fn env(cloudflare: bool) -> EnvSecrets {
    EnvSecrets {
        hcloud: Some(HCLOUD.to_owned().into()),
        cloudflare: cloudflare.then(|| CLOUDFLARE.to_owned().into()),
        admin_password: Some(PASSWORD.to_owned().into()),
    }
}

fn events(buf: &Arc<Mutex<Vec<u8>>>) -> Vec<Value> {
    String::from_utf8(buf.lock().unwrap().clone())
        .unwrap()
        .lines()
        .map(|l| {
            serde_json::from_str(l)
                .unwrap_or_else(|e| panic!("not one JSON object per line: {l}: {e}"))
        })
        .collect()
}

fn report(state: &str, steps: &[(&str, &str)]) -> Value {
    json!({
        "state": state,
        "steps": steps.iter().map(|(n, s)| json!({"name": n, "state": s, "at": 0})).collect::<Vec<_>>(),
    })
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn creates_a_server_and_logs_in(pool: PgPool) {
    let (cloud, m) = mock_cloud(pool.clone()).await;
    m.lock().unwrap().reports = vec![
        report("running", &[("Checking this server", "running")]),
        report(
            "running",
            &[
                ("Checking this server", "done"),
                ("Installing packages", "running"),
            ],
        ),
        report(
            "running",
            &[
                ("Checking this server", "done"),
                ("Installing packages", "done"),
                ("Setting up the firewall", "done"),
                ("Downloading release (latest)", "running"),
            ],
        ),
        report("handoff", &[("Checking this server", "done")]),
    ];
    let url = instance(pool.clone()).await;
    let dir = temp_dir();
    let (mut ctx, buf) = ctx(&dir);
    let mut a = args();
    a.cloudflare_dns = true;
    let code = create_with(&mut ctx, a, env(true), settings(&cloud, &m, &url, true))
        .await
        .unwrap();
    let ev = events(&buf);
    assert_eq!(code, 0, "{ev:#?}");

    // One JSON object per line: the plan, steps, milestones, then the result.
    let kinds: Vec<&str> = ev.iter().map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(kinds.first(), Some(&"plan"));
    for k in [
        "firewall_created",
        "server_created",
        "dns_record_created",
        "dns_ready",
        "setup_complete",
        "logged_in",
        "ready",
    ] {
        assert!(kinds.contains(&k), "no {k} in {kinds:?}");
    }
    let steps: Vec<String> = ev
        .iter()
        .filter(|e| e["event"] == "step" && e["state"] == "done")
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    for s in [
        "Spinning up the server",
        "Booting Ubuntu",
        "Checking the server",
        "Updating packages, installing Docker, Postgres & Redis",
        "Setting up the firewall & hardening",
        "Downloading Modula (latest)",
        "Starting services",
    ] {
        assert!(steps.iter().any(|d| d == s), "step {s} not done: {steps:?}");
    }
    let result = ev.last().unwrap();
    assert_eq!(result["event"], "result");
    assert_eq!(result["ok"], true);
    assert_eq!(result["url"], "https://cloud.test");
    assert_eq!(result["email"], "ada@example.com");

    // What Hetzner was asked for.
    {
        let c = m.lock().unwrap();
        let server = &c.servers[0];
        assert_eq!(server["image"], "ubuntu-24.04");
        assert_eq!(server["server_type"], "cx33");
        assert_eq!(server["location"], "fsn1");
        assert_eq!(server["labels"]["modula"], "1");
        assert_eq!(server["labels"]["role"], "main");
        assert_eq!(server["ssh_keys"], json!([42]));
        assert_eq!(server["firewalls"], json!([{"firewall": 100}]));
        let ports: Vec<String> = c.firewalls[0]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{}/{}",
                    r["protocol"].as_str().unwrap(),
                    r["port"].as_str().unwrap_or("-")
                )
            })
            .collect();
        assert_eq!(
            ports,
            [
                "tcp/22",
                "tcp/80",
                "tcp/443",
                "udp/443",
                "tcp/20000-29999",
                "tcp/7101",
                "icmp/-"
            ]
        );
        let user_data = server["user_data"].as_str().unwrap();
        assert!(user_data.contains("--domain 'cloud.test' --email 'ada@example.com' --name 'modula-main' --setup-token 'mcsetup_"));
        assert!(user_data.contains("--progress-token '"));
        assert!(
            !user_data.contains(HCLOUD)
                && !user_data.contains(CLOUDFLARE)
                && !user_data.contains(PASSWORD)
        );
        assert!(
            c.calls
                .iter()
                .any(|x| x == "CF create \"A\" \"127.0.0.1\" proxied=false"),
            "{:?}",
            c.calls
        );
        assert!(c.deleted_servers.is_empty());
    }

    // Logged in like `modula login`: the instance's URL and a working token.
    let saved = std::fs::read_to_string(dir.join("config.toml")).unwrap();
    assert!(saved.contains("url = \"https://cloud.test/\""), "{saved}");
    let token = saved
        .split("token = \"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert!(token.starts_with("mdt_"));
    assert!(!saved.contains(HCLOUD) && !saved.contains(PASSWORD));
    let admin: (String, bool) =
        sqlx::query_as("SELECT name, is_instance_admin FROM users WHERE email = 'ada@example.com'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(admin, ("Ada".into(), true));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn an_install_failure_deletes_the_server(pool: PgPool) {
    let (cloud, m) = mock_cloud(pool.clone()).await;
    let mut failed = report(
        "failed",
        &[
            ("Checking this server", "done"),
            ("Setting up Postgres", "running"),
        ],
    );
    failed["error"] = json!({"step": "Setting up Postgres", "message": "Postgres didn't start", "hint": "Check journalctl -u postgresql."});
    m.lock().unwrap().reports = vec![failed];
    let url = instance(pool).await;
    let dir = temp_dir();
    let (mut ctx, buf) = ctx(&dir);
    let mut a = args();
    a.ssh_key_id = None;
    a.ssh_key_file = Some({
        let p = dir.join("id_ed25519.pub");
        std::fs::write(&p, "ssh-ed25519 AAAAnew ada@laptop\n").unwrap();
        p
    });
    let code = create_with(&mut ctx, a, env(false), settings(&cloud, &m, &url, false))
        .await
        .unwrap();
    let ev = events(&buf);
    assert_eq!(code, 1, "{ev:#?}");
    let failed = ev
        .iter()
        .find(|e| e["event"] == "install_failed")
        .expect("install_failed");
    assert_eq!(failed["message"], "Postgres didn't start");
    let cleanup = ev
        .iter()
        .find(|e| e["event"] == "cleanup")
        .expect("cleanup");
    assert_eq!(cleanup["serverDeleted"], true);
    assert_eq!(cleanup["firewallDeleted"], true);
    let result = ev.last().unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["serverKept"], false);
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("Setting up Postgres: Postgres didn't start")
    );
    assert!(result["hint"].as_str().unwrap().contains("journalctl"));
    let c = m.lock().unwrap();
    assert_eq!(c.deleted_servers, [5000]);
    assert_eq!(c.deleted_firewalls, [100]);
    // The local key was uploaded first (not in the project yet).
    assert!(c.calls.contains(&"POST /ssh_keys".to_owned()));
    assert_eq!(c.servers[0]["ssh_keys"], json!([43]));
    drop(c);
    assert!(!dir.join("config.toml").exists(), "not logged in");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn without_a_terminal_missing_flags_fail_before_any_call(pool: PgPool) {
    let (cloud, m) = mock_cloud(pool.clone()).await;
    let url = instance(pool).await;
    let dir = temp_dir();
    let (mut ctx, _) = ctx(&dir);
    ctx.global.yes = false;
    let a = ServerCreateArgs {
        location: Some("fsn1".into()),
        on_failure: Some(OnFailure::Keep),
        ..Default::default()
    };
    let e = create_with(
        &mut ctx,
        a,
        EnvSecrets::default(),
        settings(&cloud, &m, &url, false),
    )
    .await
    .unwrap_err();
    assert!(e.downcast_ref::<UsageError>().is_some(), "exit code 2");
    let msg = e.to_string();
    for flag in [
        "HCLOUD_TOKEN",
        "--type",
        "--domain",
        "--email",
        "--ssh-key-file",
        "MODULA_ADMIN_PASSWORD",
        "--yes",
    ] {
        assert!(msg.contains(flag), "{flag} missing from: {msg}");
    }
    assert!(!msg.contains("--location"), "it was given");

    // A location with nothing suitable priced (hel1), or a type not sold
    // there: usage errors naming what is available; still nothing created.
    for (location, server_type, expected) in [
        ("hel1", "cx33", "Available: fsn1"),
        (
            "fsn1",
            "cx99",
            "Available there: cx23, cx33 (recommended: cx33)",
        ),
    ] {
        let (mut ctx, _) = self::ctx(&dir);
        let mut a = args();
        a.server_type = Some(server_type.into());
        a.location = Some(location.into());
        let e = create_with(&mut ctx, a, env(false), settings(&cloud, &m, &url, false))
            .await
            .unwrap_err();
        assert!(e.downcast_ref::<UsageError>().is_some());
        assert!(e.to_string().contains(expected), "{e}");
    }
    let c = m.lock().unwrap();
    assert!(
        c.servers.is_empty() && c.firewalls.is_empty(),
        "{:?}",
        c.calls
    );
    assert!(
        c.calls.iter().all(|x| x.starts_with("GET ")),
        "{:?}",
        c.calls
    );
    drop(c);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn the_cli_hashes_setup_tokens_like_the_server() {
    let t = modula_cli::provision::setup_token::generate().unwrap();
    use secrecy::ExposeSecret;
    let t = t.expose_secret();
    assert!(modula_crypto::token::is_well_formed(
        t,
        modula_crypto::token::prefix::SETUP
    ));
    assert_eq!(
        modula_cli::provision::setup_token::hash_hex(t),
        hex::encode(modula_crypto::token::hash(t).0)
    );
}

/// What people see without --json (run with `-- --ignored --nocapture`).
#[ignore = "prints the human transcript"]
#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn human_transcript(pool: PgPool) {
    let (cloud, m) = mock_cloud(pool.clone()).await;
    let all = [
        ("Checking this server", "done"),
        ("Installing packages", "done"),
        ("Setting up the firewall", "done"),
        ("Turning on security updates", "done"),
        ("Downloading release (latest)", "done"),
        ("Setting up Postgres", "done"),
        ("Setting up Redis", "done"),
    ];
    let mut reports: Vec<Value> = (1..=all.len())
        .map(|n| {
            let mut steps = all[..n].to_vec();
            steps[n - 1].1 = "running";
            report("running", &steps)
        })
        .collect();
    reports.push(report("handoff", &all));
    m.lock().unwrap().reports = reports;
    let url = instance(pool).await;
    let dir = temp_dir();
    let session = Session {
        config_path: dir.join("config.toml"),
        cwd: dir.clone(),
        env_url: None,
        env_token: None,
        interactive: false,
    };
    let global = Global {
        yes: true,
        ..Default::default()
    };
    let mut ctx = Ctx::new(session, global, Output::stdout(false)).unwrap();
    let mut s = settings(&cloud, &m, &url, true);
    s.poll = Duration::from_millis(300);
    let mut a = args();
    a.cloudflare_dns = true;
    let code = create_with(&mut ctx, a, env(true), s).await.unwrap();
    assert_eq!(code, 0);
    std::fs::remove_dir_all(&dir).unwrap();
}
