//! Resizing servers in place, against a local mock of the Hetzner Cloud API:
//! linking a node to its provider server by IP, the options, the step
//! machine, and the main server's refusal.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::*;
use modula_id::Id;
use serde_json::{Value, json};
use sqlx::PgPool;

const TOKEN: &str = "hetzner-test-token-0123456789abcdefghijklmnopqrstuvwxyz";

struct MockServer {
    id: i64,
    name: String,
    status: String,
    ip: String,
    labels: Value,
    server_type: String,
}

#[derive(Default)]
struct Mock {
    servers: Vec<MockServer>,
    /// The OS ignores ACPI shutdowns (only `poweroff` stops it).
    ignores_shutdown: bool,
    /// What `GET /actions/{id}` answers for the type change.
    change_type_result: Option<&'static str>,
    calls: Vec<String>,
    change_type_bodies: Vec<Value>,
}

type Shared = Arc<Mutex<Mock>>;

fn types() -> Value {
    let price = |loc: &str, gross: &str| json!({"location": loc, "price_hourly": {"net": "0.01", "gross": "0.01"}, "price_monthly": {"net": "1.00", "gross": gross}});
    json!([
        {"name": "cx22", "description": "CX22", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "architecture": "x86", "storage_type": "local", "deprecation": null,
         "prices": [price("fsn1", "4.5100000000"), price("hel1", "4.5100000000")]},
        {"name": "cx32", "description": "CX32", "cores": 4, "memory": 8.0, "disk": 80, "cpu_type": "shared", "architecture": "x86", "storage_type": "local", "deprecation": null,
         "prices": [price("fsn1", "8.2100000000")]},
        {"name": "cx42", "description": "CX42", "cores": 8, "memory": 16.0, "disk": 160, "cpu_type": "shared", "architecture": "x86", "storage_type": "local", "deprecation": null,
         "prices": [price("hel1", "18.9200000000")]},
        {"name": "cax21", "description": "CAX21", "cores": 4, "memory": 8.0, "disk": 80, "cpu_type": "shared", "architecture": "arm", "storage_type": "local", "deprecation": null,
         "prices": [price("fsn1", "7.4900000000")]}
    ])
}

fn server_json(s: &MockServer) -> Value {
    let ty = types()
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == s.server_type.as_str())
        .cloned()
        .unwrap();
    json!({
        "id": s.id, "name": s.name, "status": s.status,
        "public_net": {"ipv4": {"ip": s.ip}},
        "server_type": ty,
        "location": {"name": "fsn1", "description": "Falkenstein DC Park 1", "country": "DE", "city": "Falkenstein", "network_zone": "eu-central"},
        "labels": s.labels,
        "primary_disk_size": 40
    })
}

fn authorized(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

fn err(status: StatusCode, code: &str) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({"error": {"code": code, "message": code}})),
    )
}

fn page(items: Value, key: &str) -> Value {
    json!({ key: items, "meta": { "pagination": { "page": 1, "per_page": 50, "next_page": null } } })
}

async fn mock_hetzner(m: Shared) -> url::Url {
    let app = Router::new()
        .route(
            "/v1/locations",
            get(|h: HeaderMap| async move {
                if !authorized(&h) {
                    return err(StatusCode::UNAUTHORIZED, "unauthorized");
                }
                let locs = json!([{"name": "fsn1"}, {"name": "hel1"}]);
                (StatusCode::OK, Json(page(locs, "locations")))
            }),
        )
        .route(
            "/v1/server_types",
            get(|h: HeaderMap| async move {
                if !authorized(&h) {
                    return err(StatusCode::UNAUTHORIZED, "unauthorized");
                }
                (StatusCode::OK, Json(page(types(), "server_types")))
            }),
        )
        .route(
            "/v1/servers",
            get(
                |State(m): State<Shared>, h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    if !authorized(&h) {
                        return err(StatusCode::UNAUTHORIZED, "unauthorized");
                    }
                    let sel = q.get("label_selector").cloned().unwrap_or_default();
                    let mut mock = m.lock().unwrap();
                    mock.calls.push(format!("GET /servers?{sel}"));
                    let found: Vec<Value> = mock
                        .servers
                        .iter()
                        .filter(|s| {
                            sel.split(',').all(|kv| {
                                let (k, v) = kv.split_once('=').unwrap();
                                s.labels[k] == v
                            })
                        })
                        .map(server_json)
                        .collect();
                    (StatusCode::OK, Json(page(json!(found), "servers")))
                },
            ),
        )
        .route(
            "/v1/servers/{id}",
            get(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
                if !authorized(&h) {
                    return err(StatusCode::UNAUTHORIZED, "unauthorized");
                }
                let mock = m.lock().unwrap();
                match mock.servers.iter().find(|s| s.id == id) {
                    Some(s) => (StatusCode::OK, Json(json!({"server": server_json(s)}))),
                    None => err(StatusCode::NOT_FOUND, "not_found"),
                }
            }),
        )
        .route(
            "/v1/servers/{id}/actions/{action}",
            post(
                |State(m): State<Shared>, h: HeaderMap, Path((id, action)): Path<(i64, String)>, Json(body): Json<Value>| async move {
                    if !authorized(&h) {
                        return err(StatusCode::UNAUTHORIZED, "unauthorized");
                    }
                    let mut mock = m.lock().unwrap();
                    mock.calls.push(format!("POST /servers/{id}/actions/{action}"));
                    let ignores = mock.ignores_shutdown;
                    let Some(s) = mock.servers.iter_mut().find(|s| s.id == id) else {
                        return err(StatusCode::NOT_FOUND, "not_found");
                    };
                    match action.as_str() {
                        "shutdown" if !ignores => s.status = "off".into(),
                        "shutdown" => {}
                        "poweroff" => s.status = "off".into(),
                        "poweron" => s.status = "running".into(),
                        "change_type" => {
                            if s.status != "off" {
                                return err(StatusCode::UNPROCESSABLE_ENTITY, "server_not_stopped");
                            }
                            s.server_type = body["server_type"].as_str().unwrap().to_owned();
                            mock.change_type_bodies.push(body);
                            return (
                                StatusCode::CREATED,
                                Json(json!({"action": {"id": 77, "status": "running", "error": null}})),
                            );
                        }
                        _ => return err(StatusCode::NOT_FOUND, "not_found"),
                    }
                    (
                        StatusCode::CREATED,
                        Json(json!({"action": {"id": 70, "status": "running", "error": null}})),
                    )
                },
            ),
        )
        .route(
            "/v1/actions/{id}",
            get(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
                if !authorized(&h) {
                    return err(StatusCode::UNAUTHORIZED, "unauthorized");
                }
                let mock = m.lock().unwrap();
                let action = match mock.change_type_result.unwrap_or("success") {
                    "error" => json!({"id": id, "status": "error",
                                      "error": {"code": "action_failed", "message": "Action failed"}}),
                    status => json!({"id": id, "status": status, "error": null}),
                };
                (StatusCode::OK, Json(json!({"action": action})))
            }),
        )
        .with_state(m);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/v1").parse().unwrap()
}

struct Env {
    app: TestApp,
    mock: Shared,
    admin: String,
}

async fn setup(pool: PgPool) -> Env {
    let mock: Shared = Arc::new(Mutex::new(Mock::default()));
    {
        let mut m = mock.lock().unwrap();
        m.servers.push(MockServer {
            id: 4001,
            name: "modula-main".into(),
            status: "running".into(),
            ip: "203.0.113.1".into(),
            labels: json!({"modula": "1", "role": "main", "server": "modula-main"}),
            server_type: "cx22".into(),
        });
        m.servers.push(MockServer {
            id: 4002,
            name: "worker-1".into(),
            status: "running".into(),
            ip: "203.0.113.2".into(),
            labels: json!({"modula": "1"}),
            server_type: "cx22".into(),
        });
    }
    let mut cfg = config();
    cfg.hetzner_api = mock_hetzner(mock.clone()).await;
    let app = TestApp::with_config(pool, cfg);
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let r = app
        .call(
            Method::PUT,
            &format!("/api/workspaces/{ws}/integrations/hetzner"),
            Some(&admin),
            Some(json!({"token": TOKEN})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    Env { app, mock, admin }
}

async fn node(app: &TestApp, name: &str, ip: &str, main: bool) -> Id {
    let id = Id::new();
    let caps = if main {
        json!(["dashboard"])
    } else {
        json!([])
    };
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at, public_ip, info, last_seen_at)
         VALUES ($1, $2, 'online', $1::text, now() + interval '1 day', $3, $4, now() - interval '1 minute')",
    )
    .bind(id)
    .bind(name)
    .bind(ip)
    .bind(json!({ "capabilities": caps }))
    .execute(app.db())
    .await
    .unwrap();
    id
}

async fn row(app: &TestApp, node: Id) -> (String, String, Option<String>, Option<String>, String) {
    sqlx::query_as(
        "SELECT status, server_type, resize_step, error, origin FROM provisioned_servers WHERE node_id = $1",
    )
    .bind(node)
    .fetch_one(app.db())
    .await
    .unwrap()
}

/// Make a resize step look older (its waits are minutes long).
async fn age_step(app: &TestApp, node: Id, secs: i64) {
    sqlx::query(
        "UPDATE provisioned_servers SET resize_step_at = resize_step_at - make_interval(secs => $2)
         WHERE node_id = $1",
    )
    .bind(node)
    .bind(secs as f64)
    .execute(app.db())
    .await
    .unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn adopts_the_main_server_by_ip_and_refuses_to_resize_it(pool: PgPool) {
    let Env { app, mock, admin } = setup(pool).await;
    let main = node(&app, "modula-main", "203.0.113.1", true).await;

    let r = app
        .call(
            Method::GET,
            &format!("/api/nodes/{main}/resize-options"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let o = &r.json;
    assert_eq!(o["managed"], true);
    assert_eq!(o["main"], true);
    assert_eq!(o["cliCommand"], "modula server resize");
    assert_eq!(o["providerId"], "4001");
    assert_eq!(o["location"]["id"], "hetzner:fsn1");
    assert_eq!(o["current"]["name"], "cx22");
    assert_eq!(o["current"]["monthlyCents"], 451);
    let ty = |n: &str| {
        o["types"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == n)
            .unwrap()
            .clone()
    };
    assert_eq!(ty("cx22")["current"], true);
    let cx32 = ty("cx32");
    assert_eq!(
        (
            &cx32["availableHere"],
            &cx32["compatible"],
            &cx32["direction"]
        ),
        (&json!(true), &json!(true), &json!("up"))
    );
    // Sold elsewhere only: where, so the UI can offer a migration.
    let cx42 = ty("cx42");
    assert_eq!(cx42["availableHere"], false);
    assert_eq!(cx42["availableIn"][0]["id"], "hetzner:hel1");
    // Another architecture can't be switched to in place.
    let cax = ty("cax21");
    assert_eq!(cax["compatible"], false);
    assert!(cax["blocker"].as_str().unwrap().contains("architectures"));

    // Linked once, as adopted, outside the spending caps.
    let (status, server_type, _, _, origin) = row(&app, main).await;
    assert_eq!(
        (status.as_str(), server_type.as_str(), origin.as_str()),
        ("ready", "cx22", "adopted")
    );
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM provisioned_servers")
        .fetch_one(app.db())
        .await
        .unwrap();
    app.call(
        Method::GET,
        &format!("/api/nodes/{main}/resize-options"),
        Some(&admin),
        None,
    )
    .await;
    let again: i64 = sqlx::query_scalar("SELECT count(*) FROM provisioned_servers")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!((rows, again), (1, 1));

    // The control plane can't power itself off: the API points at the CLI.
    let r = app
        .call(
            Method::POST,
            &format!("/api/nodes/{main}/resize"),
            Some(&admin),
            Some(json!({"serverType": "cx32"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(
        r.json
            .to_string()
            .contains("modula server resize --type cx32")
    );
    assert!(
        !mock
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|c| c.contains("/actions/")),
        "nothing was switched off"
    );

    // A resize done by the CLI is picked up on the next look.
    mock.lock().unwrap().servers[0].server_type = "cx32".into();
    let r = app
        .call(
            Method::GET,
            &format!("/api/nodes/{main}/resize-options"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["current"]["name"], "cx32");
    let (_, server_type, ..) = row(&app, main).await;
    assert_eq!(server_type, "cx32");

    // Nor can it be deleted through the servers list.
    let id: Id = sqlx::query_scalar("SELECT id FROM provisioned_servers WHERE node_id = $1")
        .bind(main)
        .fetch_one(app.db())
        .await
        .unwrap();
    let r = app
        .call(
            Method::DELETE,
            &format!("/api/servers/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);

    // An unknown IP: not managed, with a reason.
    let stray = node(&app, "stray", "198.51.100.9", false).await;
    let r = app
        .call(
            Method::GET,
            &format!("/api/nodes/{stray}/resize-options"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["managed"], false);
    assert!(r.json["reason"].as_str().unwrap().contains("198.51.100.9"));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn resizes_a_worker_step_by_step(pool: PgPool) {
    use modula_server::provision::resize::poll_once;
    let Env { app, mock, admin } = setup(pool).await;
    mock.lock().unwrap().ignores_shutdown = true;
    let worker = node(&app, "worker-1", "203.0.113.2", false).await;
    let path = format!("/api/nodes/{worker}/resize");

    // Not sold here / not compatible / unknown are refused up front.
    for (ty, needle) in [
        ("cx42", "Helsinki"),
        ("cax21", "architectures"),
        ("zz9", "not a server type"),
    ] {
        let r = app
            .call(
                Method::POST,
                &path,
                Some(&admin),
                Some(json!({"serverType": ty})),
            )
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{ty}: {:?}", r.json);
        assert!(r.json.to_string().contains(needle), "{ty}: {:?}", r.json);
    }

    let r = app
        .call(
            Method::POST,
            &path,
            Some(&admin),
            Some(json!({"serverType": "cx32"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "resizing");
    assert_eq!(r.json["step"], "shutdown");
    assert_eq!(
        (&r.json["from"], &r.json["to"]),
        (&json!("cx22"), &json!("cx32"))
    );
    let id: Id = sqlx::query_scalar("SELECT id FROM provisioned_servers WHERE node_id = $1")
        .bind(worker)
        .fetch_one(app.db())
        .await
        .unwrap();
    // A second request while it runs is refused.
    let r = app
        .call(
            Method::POST,
            &path,
            Some(&admin),
            Some(json!({"serverType": "cx32"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);

    // The OS ignores the shutdown: still waiting, then the power is cut.
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert_eq!(row(&app, worker).await.2.as_deref(), Some("shutdown"));
    age_step(&app, worker, 120).await;
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert_eq!(row(&app, worker).await.2.as_deref(), Some("poweroff"));
    // Off: the type changes, keeping the disk.
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert_eq!(row(&app, worker).await.2.as_deref(), Some("change_type"));
    assert_eq!(
        mock.lock().unwrap().change_type_bodies,
        [json!({"server_type": "cx32", "upgrade_disk": false})]
    );
    // Done: powered on, then waiting for the node.
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert_eq!(row(&app, worker).await.2.as_deref(), Some("poweron"));
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert_eq!(row(&app, worker).await.2.as_deref(), Some("reconnect"));
    assert!(
        !poll_once(&app.state, id).await.unwrap(),
        "the node hasn't said hello yet"
    );
    sqlx::query("UPDATE nodes SET last_seen_at = now() + interval '1 second' WHERE id = $1")
        .bind(worker)
        .execute(app.db())
        .await
        .unwrap();
    assert!(poll_once(&app.state, id).await.unwrap());

    let (status, server_type, step, error, _) = row(&app, worker).await;
    assert_eq!((status.as_str(), server_type.as_str()), ("ready", "cx32"));
    assert_eq!((step, error), (None, None));
    let price: i32 =
        sqlx::query_scalar("SELECT monthly_price_cents FROM provisioned_servers WHERE id = $1")
            .bind(id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(price, 821);
    let calls = mock.lock().unwrap().calls.clone();
    let actions: Vec<&str> = calls
        .iter()
        .filter_map(|c| c.strip_prefix("POST /servers/4002/actions/"))
        .collect();
    assert_eq!(actions, ["shutdown", "poweroff", "change_type", "poweron"]);
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'server.resize' AND target_id = $1",
    )
    .bind(worker.to_string())
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // A failed type change: powered back on, ready again with the error.
    mock.lock().unwrap().change_type_result = Some("error");
    mock.lock().unwrap().ignores_shutdown = false;
    let r = app
        .call(
            Method::POST,
            &path,
            Some(&admin),
            Some(json!({"serverType": "cx22"})),
        )
        .await;
    // cx22's 40 GB disk fits: the disk was kept at 40 GB.
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert!(poll_once(&app.state, id).await.unwrap());
    let (status, _, step, error, _) = row(&app, worker).await;
    assert_eq!((status.as_str(), step), ("ready", None));
    assert!(error.unwrap().contains("Action failed"));
    let m = mock.lock().unwrap();
    assert_eq!(m.servers[1].status, "running", "powered back on");
}
