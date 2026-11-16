//! EC2 provisioning against an in-process mock of the EC2 and Pricing APIs.
//! `MODULA_AWS_API_URL`'s override sends everything to one URL; the mock
//! reads the target region from the request's signing scope.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use common::*;
use modula_id::Id;
use modula_server::provision::{ActionState, MeshSources, Phase, Power, ServerProvider};
use serde_json::{Value, json};
use sqlx::PgPool;

// The mock (`mock_aws`, `Mock`, the credentials): shared with migrate.rs.
include!("common/ec2_mock.rs");

async fn app_with_mock(pool: PgPool) -> (TestApp, Shared) {
    let (url, mock) = mock_aws().await;
    let mut cfg = config();
    cfg.aws_api = Some(url);
    (TestApp::with_config(pool, cfg), mock)
}

async fn connect(app: &TestApp, cookie: &str, ws: Id) {
    let r = app
        .call(
            Method::PUT,
            &format!("/api/workspaces/{ws}/integrations/aws"),
            Some(cookie),
            Some(json!({"accessKeyId": KEY_ID, "secretAccessKey": SECRET})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
}

async fn setup(pool: PgPool) -> (TestApp, Shared, String, Id) {
    let (app, mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    connect(&app, &admin, ws).await;
    (app, mock, admin, ws)
}

async fn provision(app: &TestApp, cookie: &str, ws: Id, body: Value) -> Resp {
    app.call(
        Method::POST,
        &format!("/api/workspaces/{ws}/servers/provision"),
        Some(cookie),
        Some(body),
    )
    .await
}

fn worker(extra: Value) -> Value {
    let mut base =
        json!({"location": "aws:us-east-1", "serverType": "t3.small", "name": "worker-1"});
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

fn calls(m: &Shared, name: &str) -> usize {
    m.lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| *c == name)
        .count()
}

async fn aws(app: &TestApp, ws: Id) -> Box<dyn ServerProvider> {
    modula_server::provision::provider_for(&app.state, ws, "aws")
        .await
        .unwrap()
        .unwrap()
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn connect_verifies_seals_and_never_returns_the_secret(pool: PgPool) {
    let (app, mock) = app_with_mock(pool).await;
    let admin = app.setup_admin().await;
    let ws = app.create_workspace(&admin, "team").await;
    let path = format!("/api/workspaces/{ws}/integrations/aws");

    let r = app
        .call(
            Method::PUT,
            &path,
            Some(&admin),
            Some(json!({"accessKeyId": "AKIAWRONGWRONGWRONG0", "secretAccessKey": SECRET})),
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
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(r.json["connected"], true);
    assert_eq!(r.json["caps"]["maxServers"], 5);
    assert!(
        r.json["iamPolicy"]["Statement"][0]["Action"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "ec2:RunInstances")
    );
    assert!(!r.json.to_string().contains(SECRET));
    let (sealed, metadata): (Vec<u8>, Value) =
        sqlx::query_as("SELECT secret_sealed, metadata FROM integrations WHERE workspace_id = $1")
            .bind(ws)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert!(!sealed.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()));
    assert_eq!(metadata["capsCurrency"], "usd", "caps are marked as USD");
    // Validated with both APIs.
    assert_eq!(calls(&mock, "GetProducts"), 1);

    let r = app
        .call(
            Method::PATCH,
            &path,
            Some(&admin),
            Some(json!({"maxServers": 2})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["caps"]["maxServers"], 2);

    let r = app.call(Method::DELETE, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["connected"], false);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_catalog_prices_compute_disk_and_address(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    let path = format!("/api/workspaces/{ws}/integrations/aws/catalog");
    let r = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let ids: Vec<&str> = r.json["locations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["location"]["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"aws:us-east-1") && ids.contains(&"aws:eu-west-1"));
    let types = r.json["serverTypes"].as_array().unwrap();
    let small = types.iter().find(|t| t["name"] == "t3.small").unwrap();
    let price = |loc: &str| {
        small["prices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["location"] == loc)
            .map(|p| p["monthlyCents"].clone())
    };
    // $0.0208/h x 730 = $15.184, + 80 GB gp3 $6.40 + address $3.65.
    assert_eq!(price("aws:us-east-1"), Some(json!(2523)));
    assert_eq!(price("aws:eu-west-1"), Some(json!(2669)));
    assert_eq!(small["arch"], "x86");
    assert_eq!(small["cpuType"], "shared");
    assert_eq!(small["diskGb"], 80);
    // Priced nowhere in eu-west-1 means not orderable there.
    let graviton = types.iter().find(|t| t["name"] == "t4g.small").unwrap();
    assert_eq!(graviton["arch"], "arm");
    assert_eq!(graviton["prices"].as_array().unwrap().len(), 1);

    // A fresh cache is served without asking again, even during an outage.
    let before = calls(&mock, "GetProducts");
    mock.lock().unwrap().fail_pricing = true;
    let again = app.call(Method::GET, &path, Some(&admin), None).await;
    assert_eq!(again.status, StatusCode::OK, "{:?}", again.json);
    assert_eq!(calls(&mock, "GetProducts"), before);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn without_any_price_cached_the_error_is_returned(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    mock.lock().unwrap().fail_pricing = true;
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/integrations/aws/catalog"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY, "{:?}", r.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn provisioning_attaches_the_address_on_poll_and_delete_releases_it(pool: PgPool) {
    use modula_server::provision::track::poll_once;
    let (app, mock, admin, ws) = setup(pool).await;

    let r = provision(
        &app,
        &admin,
        ws,
        worker(json!({"ssh": true, "diskGb": 120})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let id: Id = r.json["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(r.json["status"], "booting");
    assert_eq!(r.json["ipv4"], Value::Null, "reported only once attached");
    // 2523 for 80 GB, + 40 GB of gp3 at 8c.
    assert_eq!(r.json["monthlyPriceCents"], 2843);

    {
        let m = mock.lock().unwrap();
        let i = &m.instances[0];
        assert_eq!(i.token, id.to_string(), "launches are idempotent");
        assert_eq!(i.tags["modula-id"], id.to_string());
        assert_eq!(i.tags["Name"], "worker-1");
        assert_eq!(i.root_gb, 120);
        assert_eq!(i.groups.len(), 5, "the whole firewall set is attached");
        let form = &m.run_forms[0];
        assert_eq!(form["ImageId"], "ami-new", "the newest Ubuntu");
        assert_eq!(
            form["SubnetId"], "subnet-a",
            "a default subnet with the type"
        );
        assert_eq!(form["MetadataOptions.HttpTokens"], "required");
        assert_eq!(form["BlockDeviceMapping.1.Ebs.VolumeType"], "gp3");

        let script = &i.user_data;
        assert!(script.starts_with("#!/bin/bash\n"));
        assert!(script.contains(&format!("eip='{EIP}'")));
        assert!(script.find("public-ipv4").unwrap() < script.find("--token").unwrap());

        let base = m.groups.iter().find(|g| g.id == i.groups[0]).unwrap();
        assert_eq!(base.tags["ssh"], "1");
        assert_eq!(base.tags["workspace"], ws.to_string());
        let open = |p: &str, port: i32| {
            base.rules
                .iter()
                .any(|r| r.0 == p && r.1 == port && r.3 == "0.0.0.0/0")
        };
        assert!(open("tcp", 80) && open("tcp", 443) && open("udp", 443) && open("tcp", 22));
        assert!(open("tcp", 20000) && open("icmp", -1));

        assert_eq!(m.addresses.len(), 1);
        assert_eq!(m.addresses[0].tags["modula-id"], id.to_string());
        assert_eq!(
            m.addresses[0].tags["modula-instance"], i.id,
            "recorded after launch"
        );
    }
    assert_eq!(calls(&mock, "AssociateAddress"), 0);

    // Still pending: nothing to attach to yet.
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert_eq!(calls(&mock, "AssociateAddress"), 0);

    // Running: attached, once, and only then reported.
    mock.lock().unwrap().instances[0].state = "running".into();
    assert!(!poll_once(&app.state, id).await.unwrap());
    assert_eq!(calls(&mock, "AssociateAddress"), 1);
    let (status, ipv4): (String, Option<String>) =
        sqlx::query_as("SELECT status, ipv4 FROM provisioned_servers WHERE id = $1")
            .bind(id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(status, "joining");
    assert_eq!(ipv4.as_deref(), Some(EIP));

    let p = aws(&app, ws).await;
    let provider_id: String =
        sqlx::query_scalar("SELECT provider_id FROM provisioned_servers WHERE id = $1")
            .bind(id)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(provider_id, "us-east-1/i-0");
    let state = p.status(&provider_id, None).await.unwrap();
    assert_eq!(state.ipv4.as_deref(), Some(EIP));
    assert_eq!(calls(&mock, "AssociateAddress"), 1, "idempotent");

    let found = p.find(&id.to_string()).await.unwrap().unwrap();
    assert_eq!(found.id, provider_id);
    let by_ip = p.find_by_address(EIP).await.unwrap().unwrap();
    assert_eq!(by_ip.name, "worker-1");
    assert_eq!(by_ip.disk_gb, 120);
    assert!(p.find_by_address("192.0.2.1").await.unwrap().is_none());

    let r = app
        .call(
            Method::DELETE,
            &format!("/api/servers/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let m = mock.lock().unwrap();
    assert!(m.addresses.is_empty(), "the address no longer bills");
    assert_eq!(m.instances[0].state, "shutting-down");
    let order = |name: &str| m.calls.iter().position(|c| c == name).unwrap();
    assert!(order("TerminateInstances") < order("DisassociateAddress"));
    assert!(order("DisassociateAddress") < order("ReleaseAddress"));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_zone_out_of_capacity_launches_in_the_next(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    mock.lock().unwrap().full_subnets = vec!["subnet-a".into()];
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let id = r.json["id"].as_str().unwrap();
    let m = mock.lock().unwrap();
    let subnets: Vec<&str> = m.run_forms.iter().map(|f| f["SubnetId"].as_str()).collect();
    assert_eq!(subnets, ["subnet-a", "subnet-b"]);
    assert_eq!(m.instances.len(), 1);
    assert_eq!(
        m.instances[0].token,
        format!("{id}-1"),
        "a token of its own per zone"
    );
    assert_eq!(
        m.addresses.len(),
        1,
        "the address is kept for the launch that worked"
    );
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_failed_launch_releases_the_address(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    mock.lock().unwrap().fail_run = true;
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY, "{:?}", r.json);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("InsufficientInstanceCapacity")
    );
    assert!(mock.lock().unwrap().addresses.is_empty());
    assert_eq!(calls(&mock, "ReleaseAddress"), 1);
    assert_eq!(
        calls(&mock, "RunInstances"),
        2,
        "every zone with the type is tried"
    );
    let (status, error): (String, Option<String>) =
        sqlx::query_as("SELECT status, error FROM provisioned_servers WHERE workspace_id = $1")
            .bind(ws)
            .fetch_one(app.db())
            .await
            .unwrap();
    assert_eq!(status, "failed");
    assert!(error.unwrap().contains("capacity in any zone"));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn delete_of_a_server_already_gone_still_releases(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    // AWS forgets a terminated instance within the hour.
    mock.lock().unwrap().instances.clear();

    let p = aws(&app, ws).await;
    p.delete("us-east-1/i-0").await.unwrap();
    assert_eq!(calls(&mock, "ReleaseAddress"), 1);
    assert!(mock.lock().unwrap().addresses.is_empty());
    // And again: nothing left to do.
    p.delete("us-east-1/i-0").await.unwrap();
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_terminated_instance_stops_billing_its_address(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK);
    mock.lock().unwrap().instances[0].state = "terminated".into();
    let s = aws(&app, ws)
        .await
        .status("us-east-1/i-0", None)
        .await
        .unwrap();
    assert_eq!(s.phase, Phase::Gone);
    assert!(mock.lock().unwrap().addresses.is_empty());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn find_releases_an_address_orphaned_by_a_crash(pool: PgPool) {
    let (app, mock, _admin, ws) = setup(pool).await;
    mock.lock().unwrap().addresses.push(Addr {
        region: "eu-west-1".into(),
        allocation: "eipalloc-orphan".into(),
        ip: EIP.into(),
        instance: None,
        tags: BTreeMap::from([("modula-id".to_owned(), "crashed".to_owned())]),
    });
    assert!(aws(&app, ws).await.find("crashed").await.unwrap().is_none());
    assert!(mock.lock().unwrap().addresses.is_empty());
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_region_without_a_default_vpc_says_what_to_do(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    mock.lock().unwrap().no_vpc.push("us-east-1".into());
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY, "{:?}", r.json);
    let message = r.json["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("create-default-vpc --region us-east-1"),
        "{message}"
    );
    assert_eq!(calls(&mock, "AllocateAddress"), 0, "no address was taken");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn resize_is_a_stop_modify_and_a_done_action(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    let r = provision(&app, &admin, ws, worker(json!({"diskGb": 100}))).await;
    assert_eq!(r.status, StatusCode::OK);
    mock.lock().unwrap().instances[0].state = "running".into();
    let p = aws(&app, ws).await;
    let id = "us-east-1/i-0";
    p.status(id, None).await.unwrap();

    p.power(id, Power::Off).await.unwrap();
    let server = p.describe(id).await.unwrap().unwrap();
    assert_eq!(server.phase, Phase::Off);
    assert_eq!(server.arch, "x86");
    assert_eq!(server.disk_gb, 100, "the disk is kept");
    assert_eq!(
        server.ipv4.as_deref(),
        Some(EIP),
        "the address survives a stop"
    );

    let offers = p.server_types().await.unwrap();
    let large = offers.iter().find(|t| t.name == "t3.large").unwrap();
    let arm = offers.iter().find(|t| t.name == "t4g.small").unwrap();
    assert_eq!(p.resize_blocker(&server, large), None);
    assert!(
        p.resize_blocker(&server, arm)
            .unwrap()
            .contains("architectures")
    );

    let action = p.resize(id, "t3.large").await.unwrap();
    assert_eq!(p.action(&action).await.unwrap(), ActionState::Done);
    assert_eq!(mock.lock().unwrap().instances[0].instance_type, "t3.large");
    p.power(id, Power::On).await.unwrap();
    assert_eq!(mock.lock().unwrap().instances[0].state, "running");
    assert_eq!(calls(&mock, "AllocateAddress"), 1, "no new address");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn sync_mesh_only_changes_the_wireguard_rule_of_modula_groups(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK);
    // A group that isn't ours.
    mock.lock().unwrap().groups.push(Group {
        region: "us-east-1".into(),
        id: "sg-user".into(),
        name: "mine".into(),
        vpc: "vpc-1".into(),
        tags: BTreeMap::new(),
        rules: vec![("udp".into(), 51820, 51820, "192.0.2.9/32".into())],
    });
    let p = aws(&app, ws).await;
    let sources = MeshSources {
        workspace: (1..=56).map(|n| format!("203.0.113.{n}")).collect(),
        everyone: vec!["203.0.113.1".into()],
    };
    p.sync_mesh(&sources).await.unwrap();
    {
        let m = mock.lock().unwrap();
        let mesh = |g: &Group| {
            g.rules
                .iter()
                .filter(|r| r.0 == "udp" && r.1 == 51820)
                .count()
        };
        let of: Vec<_> = m.groups.iter().filter(|g| g.id != "sg-user").collect();
        assert_eq!(of.len(), 5);
        assert_eq!(of.iter().map(|g| mesh(g)).sum::<usize>(), 56);
        assert_eq!(mesh(of[0]), 55, "room left for the five fixed rules");
        assert_eq!(mesh(of[1]), 1, "spills into the next group");
        assert_eq!(
            m.groups
                .iter()
                .find(|g| g.id == "sg-user")
                .unwrap()
                .rules
                .len(),
            1
        );
    }

    let before = calls(&mock, "AuthorizeSecurityGroupIngress");
    p.sync_mesh(&sources).await.unwrap();
    assert_eq!(
        calls(&mock, "AuthorizeSecurityGroupIngress"),
        before,
        "in step"
    );
    assert_eq!(calls(&mock, "RevokeSecurityGroupIngress"), 0);

    // A peer leaves: exactly it goes.
    let fewer = MeshSources {
        workspace: sources.workspace[1..].to_vec(),
        ..sources.clone()
    };
    p.sync_mesh(&fewer).await.unwrap();
    assert_eq!(calls(&mock, "RevokeSecurityGroupIngress"), 1);

    let too_many = MeshSources {
        workspace: (0..400)
            .map(|n| format!("203.0.{}.{}", n / 256, n % 256))
            .collect(),
        ..fewer
    };
    let e = p.sync_mesh(&too_many).await.unwrap_err().to_string();
    assert!(e.contains("security groups hold only"), "{e}");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn promoting_replaces_the_groups_with_the_main_set(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK);
    let p = aws(&app, ws).await;
    p.open_node_port("us-east-1/i-0", "worker-1", 7101)
        .await
        .unwrap();
    let m = mock.lock().unwrap();
    let groups = &m.instances[0].groups;
    assert_eq!(groups.len(), 5);
    let base = m.groups.iter().find(|g| g.id == groups[0]).unwrap();
    assert_eq!(base.tags["role"], "main");
    assert_eq!(base.tags["server"], "worker-1");
    assert!(base.rules.iter().any(|r| r.0 == "tcp" && r.1 == 7101));
    assert!(base.rules.iter().any(|r| r.0 == "tcp" && r.1 == 22));
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_key_in_another_region_is_imported_and_extras_go_in_user_data(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    {
        let mut m = mock.lock().unwrap();
        for (region, name, material) in [
            ("eu-west-1", "laptop", "ssh-ed25519 AAAAlaptop me@laptop"),
            ("eu-west-1", "ci", "ssh-ed25519 AAAAci me@ci"),
            // The same key in two regions is listed once.
            ("us-east-1", "ci", "ssh-ed25519 AAAAci me@ci"),
        ] {
            m.keys.push(Key {
                region: region.into(),
                name: name.into(),
                material: material.into(),
            });
        }
    }
    let r = app
        .call(
            Method::GET,
            &format!("/api/workspaces/{ws}/integrations/aws/ssh-keys"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let keys = r.json.as_array().unwrap();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.iter().any(|k| k["id"] == "eu-west-1/laptop"));

    let r = provision(
        &app,
        &admin,
        ws,
        worker(json!({"sshKeyIds": ["eu-west-1/laptop", "eu-west-1/ci"]})),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    {
        let m = mock.lock().unwrap();
        assert!(
            m.keys
                .iter()
                .any(|k| k.region == "us-east-1" && k.name == "laptop")
        );
        assert_eq!(m.instances[0].key.as_deref(), Some("laptop"));
        assert!(
            m.instances[0]
                .user_data
                .contains("ssh-ed25519 AAAAci me@ci")
        );
        assert_eq!(m.calls.iter().filter(|c| *c == "ImportKeyPair").count(), 1);
    }
    let bad = provision(
        &app,
        &admin,
        ws,
        json!({"location": "aws:us-east-1", "serverType": "t3.small", "name": "worker-2", "sshKeyIds": ["nope"]}),
    )
    .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{:?}", bad.json);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn throttled_requests_are_repeated(pool: PgPool) {
    let (app, mock, _admin, ws) = setup(pool).await;
    let p = aws(&app, ws).await;
    mock.lock().unwrap().throttle = 2;
    assert_eq!(p.locations().await.unwrap().len(), 2);
    assert_eq!(mock.lock().unwrap().throttle, 0);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn a_lost_race_to_create_a_group_uses_the_winners(pool: PgPool) {
    let (app, mock, admin, ws) = setup(pool).await;
    mock.lock().unwrap().race_groups = true;
    let r = provision(&app, &admin, ws, worker(json!({}))).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let m = mock.lock().unwrap();
    assert_eq!(m.instances[0].groups.len(), 5);
    assert_eq!(m.groups.len(), 5);
}
