//! Hetzner provisioning against a local mock of the Hetzner Cloud API.

mod common;

use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use common::*;
use modula_id::Id;
use secrecy::SecretString;
use serde_json::{Value, json};
use sqlx::PgPool;

const TOKEN: &str = "hetzner-test-token-0123456789abcdefghijklmnopqrstuvwxyz";

#[derive(Default)]
struct Mock {
    firewalls: Vec<Value>,
    servers: Vec<Value>,
    deleted: Vec<i64>,
    server_status: String,
    /// Every request path, for asserting what was (not) called.
    calls: Vec<String>,
}

type Shared = Arc<Mutex<Mock>>;

fn authorized(h: &HeaderMap) -> bool {
    h.get("authorization").and_then(|v| v.to_str().ok()) == Some(&format!("Bearer {TOKEN}"))
}

fn unauthorized() -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"error": {"code": "unauthorized", "message": "unable to authenticate"}})),
    )
}

macro_rules! guard {
    ($m:expr, $h:expr, $what:expr) => {{
        $m.lock().unwrap().calls.push($what.to_string());
        if !authorized(&$h) {
            return unauthorized();
        }
    }};
}

fn page(items: Value, key: &str, next: Option<u32>) -> Value {
    json!({ key: items, "meta": { "pagination": { "page": 1, "per_page": 50, "next_page": next } } })
}

async fn mock_hetzner() -> (url::Url, Shared) {
    let m: Shared = Arc::new(Mutex::new(Mock {
        server_status: "initializing".into(),
        ..Default::default()
    }));
    let app = Router::new()
        .route(
            "/v1/locations",
            get(|State(m): State<Shared>, h: HeaderMap| async move {
                guard!(m, h, "GET /locations");
                let locs = json!([
                    {"name": "fsn1", "description": "Falkenstein DC Park 1", "country": "DE", "city": "Falkenstein", "network_zone": "eu-central"},
                    {"name": "hel1", "description": "Helsinki DC Park 1", "country": "FI", "city": "Helsinki", "network_zone": "eu-central"},
                    {"name": "ash", "description": "Ashburn, VA", "country": "US", "city": "Ashburn, VA", "network_zone": "us-east"},
                    {"name": "xyz9", "description": "Unknown", "country": "ZZ", "city": "Nowhere", "network_zone": "x"}
                ]);
                (StatusCode::OK, Json(page(locs, "locations", None)))
            }),
        )
        .route(
            "/v1/server_types",
            get(
                |State(m): State<Shared>, h: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    guard!(m, h, "GET /server_types");
                    let price = |loc: &str, gross: &str| json!({"location": loc, "price_hourly": {"net": "0.01", "gross": "0.01"}, "price_monthly": {"net": "1.00", "gross": gross}});
                    // Two pages, to exercise pagination.
                    if q.get("page").map(String::as_str) == Some("2") {
                        let types = json!([
                            {"name": "cx32", "description": "CX32", "cores": 4, "memory": 8.0, "disk": 80, "cpu_type": "shared", "architecture": "x86", "deprecation": null,
                             "prices": [price("fsn1", "8.2100000000"), price("hel1", "8.2100000000")]},
                            {"name": "cx11", "description": "CX11", "cores": 1, "memory": 2.0, "disk": 20, "cpu_type": "shared", "architecture": "x86", "deprecated": true,
                             "deprecation": {"announced": "2024-06-06T00:00:00Z", "unavailable_after": "2024-09-06T00:00:00Z"},
                             "prices": [price("fsn1", "1.0000000000")]}
                        ]);
                        return (StatusCode::OK, Json(page(types, "server_types", None)));
                    }
                    let types = json!([
                        {"name": "cx22", "description": "CX22", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "architecture": "x86", "deprecation": null,
                         "prices": [price("fsn1", "4.5101000000"), price("hel1", "4.5101000000")]},
                        {"name": "cax11", "description": "CAX11", "cores": 2, "memory": 4.0, "disk": 40, "cpu_type": "shared", "architecture": "arm", "deprecation": null,
                         "prices": [price("fsn1", "4.5100000000")]}
                    ]);
                    (StatusCode::OK, Json(page(types, "server_types", Some(2))))
                },
            ),
        )
        .route(
            "/v1/ssh_keys",
            get(|State(m): State<Shared>, h: HeaderMap| async move {
                guard!(m, h, "GET /ssh_keys");
                let keys = json!([{"id": 42, "name": "laptop", "fingerprint": "b7:2f:30"}]);
                (StatusCode::OK, Json(page(keys, "ssh_keys", None)))
            }),
        )
        .route(
            "/v1/firewalls",
            get(
                |State(m): State<Shared>, h: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    guard!(m, h, "GET /firewalls");
                    let sel = q.get("label_selector").cloned().unwrap_or_default();
                    let mock = m.lock().unwrap();
                    let found: Vec<Value> = mock
                        .firewalls
                        .iter()
                        .filter(|f| {
                            sel.split(',').all(|kv| {
                                let (k, v) = kv.split_once('=').unwrap();
                                f["labels"][k] == v
                            })
                        })
                        .cloned()
                        .collect();
                    (StatusCode::OK, Json(page(json!(found), "firewalls", None)))
                },
            )
            .post(|State(m): State<Shared>, h: HeaderMap, Json(body): Json<Value>| async move {
                guard!(m, h, "POST /firewalls");
                let mut mock = m.lock().unwrap();
                let id = 100 + mock.firewalls.len() as i64;
                let mut fw = body.clone();
                fw["id"] = json!(id);
                mock.firewalls.push(fw.clone());
                (StatusCode::CREATED, Json(json!({"firewall": fw, "actions": []})))
            }),
        )
        .route(
            "/v1/firewalls/{id}/actions/set_rules",
            post(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>, Json(body): Json<Value>| async move {
                guard!(m, h, format!("POST /firewalls/{id}/actions/set_rules"));
                let mut mock = m.lock().unwrap();
                if let Some(fw) = mock.firewalls.iter_mut().find(|f| f["id"] == id) {
                    fw["rules"] = body["rules"].clone();
                }
                (StatusCode::CREATED, Json(json!({"actions": []})))
            }),
        )
        .route(
            "/v1/servers",
            get(
                |State(m): State<Shared>, h: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>| async move {
                    guard!(m, h, "GET /servers");
                    let sel = q.get("label_selector").cloned().unwrap_or_default();
                    let mock = m.lock().unwrap();
                    let found: Vec<Value> = mock
                        .servers
                        .iter()
                        .filter(|s| {
                            sel.split(',').all(|kv| {
                                let (k, v) = kv.split_once('=').unwrap();
                                s["labels"][k] == v
                            })
                        })
                        .map(|s| {
                            json!({"id": s["id"], "name": s["name"], "status": mock.server_status,
                                   "labels": s["labels"], "public_net": s["public_net"]})
                        })
                        .collect();
                    (StatusCode::OK, Json(page(json!(found), "servers", None)))
                },
            )
            .post(|State(m): State<Shared>, h: HeaderMap, Json(body): Json<Value>| async move {
                guard!(m, h, "POST /servers");
                if body["server_type"] == "cx32" && body["location"] == "hel1" {
                    return (
                        StatusCode::PRECONDITION_FAILED,
                        Json(json!({"error": {"code": "resource_unavailable", "message": "server type unavailable"}})),
                    );
                }
                let mut mock = m.lock().unwrap();
                let id = 5000 + mock.servers.len() as i64;
                let mut stored = body.clone();
                stored["id"] = json!(id);
                mock.servers.push(stored);
                (
                    StatusCode::CREATED,
                    Json(json!({
                        "server": {"id": id, "name": body["name"], "status": "initializing",
                                   "public_net": {"ipv4": {"ip": "203.0.113.10"}}},
                        "action": {"id": 900 + id, "status": "running", "progress": 0, "error": null},
                        "next_actions": [],
                        // Returned when no SSH key is given; must never be stored.
                        "root_password": "ROOT-PASSWORD-NEVER-STORED"
                    })),
                )
            }),
        )
        .route(
            "/v1/servers/{id}",
            get(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
                guard!(m, h, format!("GET /servers/{id}"));
                let mock = m.lock().unwrap();
                if mock.deleted.contains(&id) || !mock.servers.iter().any(|s| s["id"] == id) {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": {"code": "not_found", "message": "server not found"}})),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({"server": {"id": id, "name": "x", "status": mock.server_status,
                                           "public_net": {"ipv4": {"ip": "203.0.113.10"}}}})),
                )
            })
            .delete(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
                guard!(m, h, format!("DELETE /servers/{id}"));
                m.lock().unwrap().deleted.push(id);
                (StatusCode::OK, Json(json!({"action": {"id": 1, "status": "running"}})))
            }),
        )
        .route(
            "/v1/actions/{id}",
            get(|State(m): State<Shared>, h: HeaderMap, Path(id): Path<i64>| async move {
                guard!(m, h, format!("GET /actions/{id}"));
                (StatusCode::OK, Json(json!({"action": {"id": id, "status": "success", "error": null}})))
            }),
        )
        .with_state(m.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1").parse().unwrap(), m)
}

async fn app_with_mock(pool: PgPool) -> (TestApp, Shared) {
    let (url, mock) = mock_hetzner().await;
    let mut cfg = config();
    cfg.hetzner_api = url;
    (TestApp::with_config(pool, cfg), mock)
}

async fn connect(app: &TestApp, cookie: &str, ws: Id) {
    let r = app
        .call(
            axum::http::Method::PUT,
            &format!("/api/workspaces/{ws}/integrations/hetzner"),
            Some(cookie),
            Some(json!({"token": TOKEN})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}

async fn provision(app: &TestApp, cookie: &str, ws: Id, body: Value) -> Resp {
    app.call(
        axum::http::Method::POST,
        &format!("/api/workspaces/{ws}/servers/provision"),
        Some(cookie),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn client_reads_locations_types_and_pages() {
    let (url, _mock) = mock_hetzner().await;
    let c = modula_server::integrations::hetzner::client::Client::new(
        url.clone(),
        SecretString::from(TOKEN.to_owned()),
    )
    .unwrap();
    let locs = c.locations().await.unwrap();
    assert_eq!(locs.len(), 4);
    let types = c.server_types().await.unwrap();
    let names: Vec<_> = types.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["cx22", "cax11", "cx32", "cx11"], "both pages read");
    assert!(types[3].is_deprecated());

    let bad = modula_server::integrations::hetzner::client::Client::new(
        url,
        SecretString::from("wrong-token-wrong-token".to_owned()),
    )
    .unwrap();
    assert!(matches!(
        bad.locations().await,
        Err(modula_server::provision::ProviderError::Unauthorized)
    ));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn connect_verifies_seals_and_requires_admin_and_sudo(pool: PgPool) {
    let (app, _mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let path = format!("/api/workspaces/{ws}/integrations/hetzner");

    let r = app
        .call(
            axum::http::Method::PUT,
            &path,
            Some(&admin),
            Some(json!({"token": "wrong-token-wrong-token"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rejected")
    );

    connect(&app, &admin, ws).await;
    let r = app
        .call(axum::http::Method::GET, &path, Some(&admin), None)
        .await;
    assert_eq!(r.json["connected"], true);
    assert_eq!(r.json["caps"]["maxServers"], 5);
    assert!(!r.json.to_string().contains(TOKEN));
    let sealed: Vec<u8> =
        sqlx::query_scalar("SELECT secret_sealed FROM integrations WHERE workspace_id = $1")
            .bind(ws)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(
        !sealed.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()),
        "stored sealed"
    );
    let audited: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE workspace_id = $1 AND action = 'integration.hetzner.connect'",
    )
    .bind(ws)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(audited, 1);

    // A member can't connect, change caps or provision; a viewer can look.
    let (member_id, member) = app.user("member@example.com").await;
    app.add_member(ws, member_id, "member").await;
    let r = app
        .call(
            axum::http::Method::PUT,
            &path,
            Some(&member),
            Some(json!({"token": TOKEN})),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = provision(
        &app,
        &member,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "w1"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = app
        .call(axum::http::Method::GET, &path, Some(&member), None)
        .await;
    assert_eq!(r.status, StatusCode::OK);

    // Changing the token, caps or provisioning needs sudo.
    app.drop_sudo().await;
    let r = app
        .call(
            axum::http::Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({"maxServers": 9})),
        )
        .await;
    assert_eq!(r.code(), "sudo_required");
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "w1"}),
    )
    .await;
    assert_eq!(r.code(), "sudo_required");
    let r = app
        .call(axum::http::Method::DELETE, &path, Some(&admin), None)
        .await;
    assert_eq!(r.code(), "sudo_required");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn provisions_with_labels_firewall_and_single_use_join(pool: PgPool) {
    let (app, mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;

    let catalog = app
        .call(
            axum::http::Method::GET,
            &format!("/api/workspaces/{ws}/integrations/hetzner/catalog"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(catalog.status, StatusCode::OK, "{:?}", catalog.json);
    let types: Vec<&str> = catalog.json["serverTypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(!types.contains(&"cx11"), "deprecated types are hidden");
    let cx22 = catalog.json["serverTypes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "cx22")
        .unwrap();
    assert_eq!(
        cx22["prices"][0],
        json!({"location": "hetzner:fsn1", "monthlyCents": 488})
    );
    assert_eq!(
        (cx22["cores"].as_u64(), cx22["arch"].as_str()),
        (Some(2), Some("x86"))
    );
    let locs: Vec<&str> = catalog.json["locations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["location"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        locs,
        ["hetzner:fsn1", "hetzner:hel1", "hetzner:ash"],
        "mapped to our catalog"
    );

    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "worker-1", "sshKeyIds": ["42"]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "booting");
    assert_eq!(r.json["monthlyPriceCents"], 488);
    assert_eq!(r.json["location"]["id"], "hetzner:fsn1");
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();

    let (server, firewall) = {
        let m = mock.lock().unwrap();
        (m.servers[0].clone(), m.firewalls[0].clone())
    };
    assert_eq!(server["name"], "worker-1");
    assert_eq!(server["server_type"], "cx22");
    assert_eq!(server["location"], "fsn1");
    assert_eq!(server["image"], "ubuntu-24.04");
    assert_eq!(server["ssh_keys"], json!([42]));
    assert_eq!(
        server["labels"],
        json!({"modula": "1", "workspace": ws.to_string(), "modula-id": id.to_string()})
    );
    assert_eq!(server["firewalls"], json!([{"firewall": firewall["id"]}]));
    // Firewall: 80, 443 (tcp+udp), the TCP proxy range and ping; no SSH by default.
    let ports: Vec<String> = firewall["rules"]
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
        ["tcp/80", "tcp/443", "udp/443", "tcp/20000-29999", "icmp/-"]
    );
    assert!(
        firewall["rules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["direction"] == "in")
    );

    // User data: the worker join with a fresh single-use token and the CA pin.
    let user_data = server["user_data"].as_str().unwrap();
    let fingerprint = app.state.pki().await.unwrap().fingerprint();
    assert!(user_data.starts_with("#!/bin/bash\n"));
    assert!(user_data.contains(&format!("--ca '{fingerprint}'")));
    assert!(user_data.contains("--server 'https://localhost:7101'"));
    assert!(user_data.contains("'https://cloud.test/api/install.sh'"));
    assert!(user_data.contains(" join "));
    let token = user_data
        .split("--token '")
        .nth(1)
        .unwrap()
        .split('\'')
        .next()
        .unwrap();
    assert!(token.starts_with("mcj_"));
    let (used, minutes): (bool, f64) = sqlx::query_as(
        "SELECT used_at IS NOT NULL, extract(epoch FROM expires_at - now())::float8 / 60
         FROM node_join_tokens t JOIN provisioned_servers p ON p.join_token_id = t.id
         WHERE p.id = $1 AND t.token_hash = $2",
    )
    .bind(id)
    .bind(modula_crypto::token::hash(token).as_bytes())
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(!used);
    assert!(
        (29.0..=30.0).contains(&minutes),
        "30-minute token, got {minutes}"
    );

    // The root password Hetzner returns is never kept.
    let row: Value =
        sqlx::query_scalar("SELECT to_jsonb(p) FROM provisioned_servers p WHERE id = $1")
            .bind(id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(!row.to_string().contains("ROOT-PASSWORD"));
    assert!(!r.json.to_string().contains("ROOT-PASSWORD"));
    assert!(
        !r.json.to_string().contains("mcj_"),
        "the token is never returned"
    );

    // Tracking: booting → joining → ready once the node enrolls and connects.
    use modula_server::provision::track::poll_once;
    assert!(!poll_once(&app.state, id).await.unwrap());
    mock.lock().unwrap().server_status = "running".into();
    assert!(!poll_once(&app.state, id).await.unwrap());
    let status: String = sqlx::query_scalar("SELECT status FROM provisioned_servers WHERE id = $1")
        .bind(id)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(status, "joining");
    let node = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at) VALUES ($1, 'worker-1', 'online', 'abc', now() + interval '1 day')",
    )
    .bind(node)
    .execute(app.db())
    .await
    .unwrap();
    sqlx::query(
        "UPDATE node_join_tokens SET used_at = now(), node_id = $2
         WHERE id = (SELECT join_token_id FROM provisioned_servers WHERE id = $1)",
    )
    .bind(id)
    .bind(node)
    .execute(app.db())
    .await
    .unwrap();
    assert!(poll_once(&app.state, id).await.unwrap());
    let r = app
        .call(
            axum::http::Method::GET,
            &format!("/api/servers/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["status"], "ready");
    assert_eq!(r.json["nodeId"], node.to_string());

    // A second server with SSH gets the SSH firewall (created once, reused).
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:hel1", "serverType": "cx22", "name": "worker-2", "ssh": true}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:hel1", "serverType": "cx22", "name": "worker-3", "ssh": true}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let m = mock.lock().unwrap();
    assert_eq!(m.firewalls.len(), 2);
    assert!(m.firewalls[1]["rules"].to_string().contains("\"22\""));
    assert_eq!(
        m.servers[2]["firewalls"],
        json!([{"firewall": m.firewalls[1]["id"]}])
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn caps_and_validation_are_enforced(pool: PgPool) {
    let (app, mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;

    // Not connected yet.
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "w1"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    connect(&app, &admin, ws).await;

    for (body, what) in [
        (
            json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "Bad Name"}),
            "name",
        ),
        (
            json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "x';reboot;'"}),
            "quote",
        ),
        (
            json!({"location": "gcp:us-central1", "serverType": "cx22", "name": "w1"}),
            "a location no provider sells",
        ),
        (
            json!({"location": "hetzner:fsn1", "serverType": "cx11", "name": "w1"}),
            "deprecated type",
        ),
        (
            json!({"location": "hetzner:ash", "serverType": "cx22", "name": "w1"}),
            "no price there",
        ),
    ] {
        let r = provision(&app, &admin, ws, body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{what}: {:?}", r.json);
    }
    assert!(mock.lock().unwrap().servers.is_empty());

    let caps = format!("/api/workspaces/{ws}/integrations/hetzner");
    let r = app
        .call(
            axum::http::Method::PATCH,
            &caps,
            Some(&admin),
            Some(json!({"maxServers": 2, "maxMonthlyCents": 1000})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);

    // Over the monthly limit: $8.87 + $4.88 > $10.
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx32", "name": "big"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "small"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("limit")
    );

    // Over the server count.
    app.call(
        axum::http::Method::PATCH,
        &caps,
        Some(&admin),
        Some(json!({"maxServers": 1, "maxMonthlyCents": 100000})),
    )
    .await;
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "small"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("at most 1")
    );

    // Hetzner refusing the order: recorded as failed, token spent.
    app.call(
        axum::http::Method::PATCH,
        &caps,
        Some(&admin),
        Some(json!({"maxServers": 5})),
    )
    .await;
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:hel1", "serverType": "cx32", "name": "nope"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY, "{:?}", r.json);
    let (status, expired): (String, bool) = sqlx::query_as(
        "SELECT p.status, t.expires_at <= now() FROM provisioned_servers p
         JOIN node_join_tokens t ON t.id = p.join_token_id WHERE p.name = 'nope'",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((status.as_str(), expired), ("failed", true));
    assert_eq!(mock.lock().unwrap().servers.len(), 1, "only 'big' exists");

    // Disconnecting is refused while servers created through it exist.
    let r = app
        .call(axum::http::Method::DELETE, &caps, Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn delete_refuses_while_services_run_unless_forced(pool: PgPool) {
    let (app, mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "worker-1"}),
    )
    .await;
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    let hetzner_id: i64 = r.json["providerId"].as_str().unwrap().parse().unwrap();

    // Joined, and running a service.
    let chain = app.seed_chain(ws).await;
    sqlx::query("UPDATE provisioned_servers SET node_id = $2, status = 'ready' WHERE id = $1")
        .bind(id)
        .bind(chain.node)
        .execute(app.db())
        .await
        .unwrap();

    let (member_id, member) = app.user("member@example.com").await;
    app.add_member(ws, member_id, "member").await;
    let path = format!("/api/servers/{id}");
    let r = app
        .call(axum::http::Method::DELETE, &path, Some(&member), None)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);

    let r = app
        .call(axum::http::Method::DELETE, &path, Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("move them")
    );
    assert!(mock.lock().unwrap().deleted.is_empty());

    let r = app
        .call(
            axum::http::Method::DELETE,
            &format!("{path}?force=true"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(mock.lock().unwrap().deleted, [hetzner_id]);
    let node_status: String = sqlx::query_scalar("SELECT status FROM nodes WHERE id = $1")
        .bind(chain.node)
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(node_status, "revoked");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM provisioned_servers")
        .fetch_one(app.db())
        .await
        .unwrap();
    assert_eq!(left, 0);
    let audited: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'server.delete'")
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(audited, 1);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn suggests_existing_room_or_the_cheapest_type(pool: PgPool) {
    let (app, _mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let suggest = |q: &str| format!("/api/workspaces/{ws}/servers/suggest?{q}");

    let r = app
        .call(
            axum::http::Method::GET,
            &suggest("cpuMillis=500&memoryMb=512"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert!(r.json["suggestion"].is_null());
    assert!(
        r.json["reason"]
            .as_str()
            .unwrap()
            .contains("connect Hetzner")
    );

    connect(&app, &admin, ws).await;
    let r = app
        .call(
            axum::http::Method::GET,
            &suggest("cpuMillis=3000&memoryMb=2048&location=hetzner:fsn1"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(
        r.json["suggestion"]["serverType"]["name"], "cx32",
        "{:?}",
        r.json
    );
    assert_eq!(r.json["suggestion"]["monthlyCents"], 887);
    assert_eq!(r.json["suggestion"]["withinCaps"], true);
    let r = app
        .call(
            axum::http::Method::GET,
            &suggest("cpuMillis=500&memoryMb=512"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.json["suggestion"]["location"], "hetzner:fsn1");
    // cax11 is 1 cent cheaper than cx22 in the mock.
    assert_eq!(r.json["suggestion"]["serverType"]["name"], "cax11");

    // A server with room: no suggestion, just where it fits.
    let node = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at, location, info)
         VALUES ($1, 'big', 'online', 'x', now() + interval '1 day', 'hetzner:fsn1',
                 '{\"cpus\": 8, \"memoryBytes\": 17179869184}')",
    )
    .bind(node)
    .execute(app.db())
    .await
    .unwrap();
    let r = app
        .call(
            axum::http::Method::GET,
            &suggest("cpuMillis=3000&memoryMb=2048&location=hetzner:fsn1"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(
        r.json["fits"][0]["nodeId"],
        node.to_string(),
        "{:?}",
        r.json
    );
    assert_eq!(r.json["fits"][0]["freeMemoryMb"], 16384 - 512);
    assert!(r.json["suggestion"].is_null());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn workspace_delete_refuses_while_servers_exist(pool: PgPool) {
    let (app, _mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "worker-1"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let server = r.json["id"].as_str().unwrap().to_owned();

    // The server would keep billing with nothing left to track it.
    let path = format!("/api/workspaces/{ws}");
    let r = app
        .call(axum::http::Method::DELETE, &path, Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "{:?}", r.json);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("servers first")
    );

    let r = app
        .call(
            axum::http::Method::DELETE,
            &format!("/api/servers/{server}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = app
        .call(axum::http::Method::DELETE, &path, Some(&admin), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}

fn mesh_sources(fw: &Value) -> Option<Vec<String>> {
    fw["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["description"] == "Modula private network")
        .map(|r| {
            assert_eq!(
                (r["protocol"].as_str(), r["port"].as_str()),
                (Some("udp"), Some("51820"))
            );
            r["source_ips"]
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s.as_str().unwrap().to_owned())
                .collect()
        })
}

async fn mesh_node(app: &TestApp, ws: Option<Id>, ip: &str) -> Id {
    let id = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, workspace_id, name, status, cert_serial, cert_expires_at, public_ip, wg_public_key)
         VALUES ($1, $2, 'n', 'online', $1::text, now() + interval '1 day', $3, 'k')",
    )
    .bind(id)
    .bind(ws)
    .bind(ip)
    .execute(app.db())
    .await
    .unwrap();
    id
}

async fn sync_firewalls(app: &TestApp) {
    let job = modula_server::jobs::Job {
        id: 0,
        kind: modula_server::provision::mesh::SYNC_JOB.into(),
        payload: json!({}),
        attempts: 0,
        max_attempts: 3,
    };
    modula_server::provision::mesh::sync_job(app.state.clone(), job)
        .await
        .unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn wireguard_is_open_only_between_modulas_servers(pool: PgPool) {
    let (app, mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let other = app.create_workspace(&admin, "other").await;
    connect(&app, &admin, ws).await;
    // The main server (shared) and its CLI-made firewall in the same project.
    let main_node = mesh_node(&app, None, "198.51.100.1").await;
    sqlx::query("UPDATE nodes SET info = '{\"capabilities\": [\"dashboard\"]}' WHERE id = $1")
        .bind(main_node)
        .execute(app.db())
        .await
        .unwrap();
    let ssh = json!([{"direction": "in", "protocol": "tcp", "port": "22",
                      "source_ips": ["0.0.0.0/0", "::/0"], "description": "SSH"}]);
    {
        let mut m = mock.lock().unwrap();
        let labels = json!({"modula": "1", "role": "main", "server": "main"});
        m.servers
            .push(json!({"id": 4000, "name": "main", "labels": labels.clone(),
                              "public_net": {"ipv4": {"ip": "198.51.100.1"}}}));
        m.firewalls.push(json!({
            "id": 900, "name": "modula-main", "labels": labels.clone(), "rules": ssh.clone(),
            "applied_to": [{"type": "server", "server": {"id": 4000}}],
        }));
        // Another instance's main server in the same project, labelled (and
        // named) just like ours: never ours to change.
        m.servers
            .push(json!({"id": 4001, "name": "main", "labels": labels.clone(),
                              "public_net": {"ipv4": {"ip": "198.51.100.77"}}}));
        m.firewalls.push(json!({
            "id": 901, "name": "modula-main", "labels": labels, "rules": ssh.clone(),
            "applied_to": [{"type": "server", "server": {"id": 4001}}],
        }));
    }
    // A worker's firewall opens WireGuard to the servers already there.
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "worker-1"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let worker_fw = |m: &Mock| {
        m.firewalls
            .iter()
            .find(|f| f["id"] != 900 && f["id"] != 901)
            .unwrap()
            .clone()
    };
    assert_eq!(
        mesh_sources(&worker_fw(&mock.lock().unwrap())),
        Some(vec!["198.51.100.1/32".to_owned()])
    );

    // The worker joins (with the address Hetzner gave it), so do a server of
    // the workspace's own and another workspace's. Each firewall lets in the
    // servers that peer with the ones behind it.
    mesh_node(&app, Some(ws), "203.0.113.10").await;
    let joined = mesh_node(&app, Some(ws), "203.0.113.9").await;
    mesh_node(&app, Some(other), "192.0.2.50").await;
    sync_firewalls(&app).await;
    {
        let m = mock.lock().unwrap();
        assert_eq!(
            mesh_sources(&worker_fw(&m)),
            Some(vec![
                "198.51.100.1/32".to_owned(),
                "203.0.113.10/32".to_owned(),
                "203.0.113.9/32".to_owned()
            ])
        );
        let main = m.firewalls.iter().find(|f| f["id"] == 900).unwrap();
        assert_eq!(
            mesh_sources(main),
            Some(vec![
                "192.0.2.50/32".to_owned(),
                "198.51.100.1/32".to_owned(),
                "203.0.113.10/32".to_owned(),
                "203.0.113.9/32".to_owned()
            ])
        );
        // Its other rules are kept.
        assert_eq!(main["rules"][0]["description"], "SSH");
        let foreign = m.firewalls.iter().find(|f| f["id"] == 901).unwrap();
        assert_eq!(
            foreign["rules"], ssh,
            "another instance's main firewall is left alone"
        );
    }
    // Unchanged: no API writes.
    let writes = |m: &Mock| m.calls.iter().filter(|c| c.ends_with("set_rules")).count();
    let before = writes(&mock.lock().unwrap());
    sync_firewalls(&app).await;
    assert_eq!(writes(&mock.lock().unwrap()), before);

    // A revoked server leaves them (the revoke queues the sync).
    let mut tx = app.db().begin().await.unwrap();
    modula_server::nodes::revoke(&mut tx, joined).await.unwrap();
    tx.commit().await.unwrap();
    let queued: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs WHERE kind = 'provision.sync_mesh' AND status = 'queued'",
    )
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(queued, 1);
    sync_firewalls(&app).await;
    assert_eq!(
        mesh_sources(&worker_fw(&mock.lock().unwrap())),
        Some(vec![
            "198.51.100.1/32".to_owned(),
            "203.0.113.10/32".to_owned()
        ])
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_location_decides_the_provider(pool: PgPool) {
    let (app, _mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;
    let list = app
        .call(
            axum::http::Method::GET,
            &format!("/api/workspaces/{ws}/servers"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(list.status, StatusCode::OK, "{:?}", list.json);
    assert_eq!(list.json["connectedProviders"][0]["kind"], "hetzner");

    // In the catalog, but nothing can create servers there yet.
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "gcp:us-central1", "serverType": "t3.small", "name": "w"}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Hetzner or AWS location")
    );
    // A key the provider doesn't hold never reaches the order.
    let r = provision(
        &app,
        &admin,
        ws,
        json!({"location": "hetzner:fsn1", "serverType": "cx22", "name": "w", "sshKeyIds": ["7"]}),
    )
    .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn euro_limits_read_as_dollars_until_saved(pool: PgPool) {
    let (app, _mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;
    sqlx::query(
        "UPDATE integrations SET metadata = '{\"maxServers\": 3, \"maxMonthlyCents\": 1000}'
         WHERE workspace_id = $1 AND kind = 'hetzner'",
    )
    .bind(ws)
    .execute(app.db())
    .await
    .unwrap();
    let path = format!("/api/workspaces/{ws}/integrations/hetzner");
    let r = app
        .call(axum::http::Method::GET, &path, Some(&admin), None)
        .await;
    assert_eq!(r.json["caps"]["maxMonthlyCents"], 1080);

    // Saving another field keeps the limit, now in dollars, without drift.
    let r = app
        .call(
            axum::http::Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({"maxServers": 4})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let r = app
        .call(axum::http::Method::GET, &path, Some(&admin), None)
        .await;
    assert_eq!(r.json["caps"]["maxMonthlyCents"], 1080);
    assert_eq!(r.json["caps"]["maxServers"], 4);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_broken_connection_does_not_stop_the_mesh_sync(pool: PgPool) {
    let (app, mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let broken = app.create_workspace(&admin, "broken").await;
    let working = app.create_workspace(&admin, "working").await;
    connect(&app, &admin, broken).await;
    connect(&app, &admin, working).await;
    mesh_node(&app, Some(working), "198.51.100.7").await;
    let selector_labels = json!({"modula": "1", "workspace": working.to_string(), "ssh": "0"});
    mock.lock().unwrap().firewalls.push(json!({
        "id": 901, "name": "modula-working", "labels": selector_labels, "rules": [],
    }));
    sqlx::query("UPDATE integrations SET secret_sealed = '\\x00' WHERE workspace_id = $1")
        .bind(broken)
        .execute(app.db())
        .await
        .unwrap();

    let job = modula_server::jobs::Job {
        id: 0,
        // A job queued under the old kind runs the same handler.
        kind: modula_server::provision::mesh::LEGACY_SYNC_JOB.into(),
        payload: json!({}),
        attempts: 0,
        max_attempts: 3,
    };
    let err = modula_server::provision::mesh::sync_job(app.state.clone(), job)
        .await
        .unwrap_err();
    assert!(err.to_string().contains(&broken.to_string()), "{err}");

    let m = mock.lock().unwrap();
    let fw = m.firewalls.iter().find(|f| f["id"] == 901).unwrap();
    assert_eq!(fw["rules"][0]["source_ips"], json!(["198.51.100.7/32"]));
}
