//! Custom domains: validation, uniqueness, and that only a verified domain is
//! routed (and therefore allowed a certificate by the node's `ask`). DNS is a
//! static table; desired state is built straight from the database.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use axum::http::{Method, StatusCode};
use common::*;
use modula_id::Id;
use modula_node::edge::{ask, normalize_host};
use modula_server::domains::dns::{RecordKind, StaticDns};
use serde_json::json;
use sqlx::PgPool;

struct Fixture {
    app: TestApp,
    admin: String,
    instance: String,
    node: Id,
    dns: Arc<StaticDns>,
}

async fn fixture(pool: PgPool) -> Fixture {
    let app = TestApp::new(pool);
    let dns = Arc::new(StaticDns::default());
    app.state.set_dns_resolver(dns.clone());
    let admin = app.setup_admin().await;
    let workspace = app.create_workspace(&admin, "team").await;
    let p = app
        .call(
            Method::POST,
            &format!("/api/workspaces/{workspace}/projects"),
            Some(&admin),
            Some(json!({"name": "shop"})),
        )
        .await;
    assert_eq!(p.status, StatusCode::OK, "{:?}", p.json);
    let project = p.json["id"].as_str().unwrap().to_owned();
    let s = app
        .call(
            Method::POST,
            &format!("/api/projects/{project}/services"),
            Some(&admin),
            Some(json!({ "name": "api", "config": { "source": { "type": "image", "image": "nginx:alpine" }, "port": 8080 } })),
        )
        .await;
    assert_eq!(s.status, StatusCode::OK, "{:?}", s.json);
    let instance = s.json["instances"][0]["id"].as_str().unwrap().to_owned();

    // A node with the service running on it.
    let node = Id::new();
    sqlx::query(
        "INSERT INTO nodes (id, name, status, cert_serial, cert_expires_at, public_ip)
         VALUES ($1, 'worker-1', 'online', $2, now() + interval '1 day', '203.0.113.7')",
    )
    .bind(node)
    .bind(node.to_string())
    .execute(app.db())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO deployments (id, service_instance_id, node_id, status, image, config)
         SELECT $1, i.id, $3, 'active', 'nginx:alpine', i.config FROM service_instances i WHERE i.id = $2",
    )
    .bind(Id::new())
    .bind(&instance)
    .bind(node)
    .execute(app.db())
    .await
    .unwrap();
    Fixture {
        app,
        admin,
        instance,
        node,
        dns,
    }
}

impl Fixture {
    async fn add(&self, hostname: &str, port: Option<u16>) -> Resp {
        self.app
            .call(
                Method::POST,
                &format!("/api/service-instances/{}/custom-domains", self.instance),
                Some(&self.admin),
                Some(json!({ "hostname": hostname, "targetPort": port })),
            )
            .await
    }

    async fn check(&self, id: &str) -> Resp {
        // Skip the "Check now" cooldown between checks in one test.
        sqlx::query("UPDATE domains SET checked_at = NULL")
            .execute(self.app.db())
            .await
            .unwrap();
        self.app
            .call(
                Method::POST,
                &format!("/api/domains/{id}/check"),
                Some(&self.admin),
                None,
            )
            .await
    }

    /// (hostname, port) routes in the node's desired state.
    async fn routes(&self) -> Vec<(String, u32)> {
        let ds = modula_server::deploy::desired::build(&self.app.state, self.node)
            .await
            .unwrap();
        ds.routes
            .into_iter()
            .map(|r| (r.hostname, r.port))
            .collect()
    }

    /// What the node's `ask` endpoint answers for `host`, given the routes it
    /// would receive now (the Caddy edge builds its allowlist the same way).
    async fn ask(&self, host: &str) -> u16 {
        let allowed: HashSet<String> = self
            .routes()
            .await
            .iter()
            .filter_map(|(h, _)| normalize_host(h))
            .collect();
        let me = "127.0.0.1:7102";
        let req = format!("GET /ask?domain={host} HTTP/1.1\r\nHost: {me}\r\n\r\n");
        ask::decide(&req, &allowed, me)
    }
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn only_verified_custom_domains_are_routed(pool: PgPool) {
    let f = fixture(pool).await;
    let g = f
        .app
        .call(
            Method::POST,
            &format!("/api/service-instances/{}/domains", f.instance),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(g.status, StatusCode::OK, "{:?}", g.json);
    let generated = g.json["hostname"].as_str().unwrap().to_owned();
    assert_eq!(g.json["status"], "verified");

    let r = f.add("WWW.Shop.com", Some(3000)).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "pending");
    assert_eq!(r.json["hostname"], "www.shop.com");
    assert_eq!(r.json["rateLimitEnforced"], false);
    let id = r.json["id"].as_str().unwrap().to_owned();
    let records = r.json["dnsRecords"].as_array().unwrap();
    assert_eq!(records[0]["type"], "CNAME");
    assert_eq!(records[0]["value"], generated.as_str());
    assert_eq!(records[1]["type"], "TXT");
    assert_eq!(records[1]["name"], "_modula-challenge.www.shop.com");
    let token = records[1]["value"].as_str().unwrap().to_owned();
    assert!(token.starts_with("modula-verify="), "{token}");

    // Pending: not routed, no certificate.
    assert_eq!(f.routes().await, vec![(generated.clone(), 8080)]);
    assert_eq!(f.ask("www.shop.com").await, 403);
    assert_eq!(f.ask(&generated).await, 200);

    // No TXT yet: still waiting, with a reason.
    let r = f.check(&id).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["status"], "pending");
    assert!(
        r.json["lastError"]
            .as_str()
            .unwrap()
            .contains("No TXT record"),
        "{:?}",
        r.json
    );

    // A wrong value fails and is still not routed.
    f.dns
        .set(RecordKind::Txt, "_modula-challenge.www.shop.com", &["nope"]);
    let r = f.check(&id).await;
    assert_eq!(r.json["status"], "failed");
    assert_eq!(f.ask("www.shop.com").await, 403);

    // The right token verifies it: routed to the chosen port, allowed by ask.
    f.dns.set(
        RecordKind::Txt,
        "_modula-challenge.www.shop.com",
        &[token.as_str()],
    );
    f.dns
        .set(RecordKind::Cname, "www.shop.com", &[generated.as_str()]);
    let r = f.check(&id).await;
    assert_eq!(r.json["status"], "verified", "{:?}", r.json);
    assert_eq!(r.json["dnsTargetOk"], true);
    assert!(r.json["lastError"].is_null());
    assert!(
        f.routes()
            .await
            .contains(&("www.shop.com".to_owned(), 3000))
    );
    assert_eq!(f.ask("www.shop.com").await, 200);
    assert_eq!(f.ask("WWW.SHOP.COM.").await, 200);

    // The check is throttled.
    let again = f
        .app
        .call(
            Method::POST,
            &format!("/api/domains/{id}/check"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(again.status, StatusCode::TOO_MANY_REQUESTS);

    // Edge settings are stored (rate limit reported as not enforced) and the
    // port change reaches the route.
    let r = f
        .app
        .call(
            Method::PATCH,
            &format!("/api/domains/{id}"),
            Some(&f.admin),
            Some(json!({ "targetPort": null, "rateLimitPerMinute": 600 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(r.json["rateLimitPerMinute"], 600);
    assert!(
        f.routes()
            .await
            .contains(&("www.shop.com".to_owned(), 8080))
    );

    // Every write is audited.
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE target_type = 'domain' ORDER BY id")
            .fetch_all(f.app.db())
            .await
            .unwrap();
    for a in ["domain.create", "domain.verify", "domain.update"] {
        assert!(actions.iter().any(|x| x == a), "{a} missing: {actions:?}");
    }

    // Deleting removes the route.
    let r = f
        .app
        .call(
            Method::DELETE,
            &format!("/api/domains/{id}"),
            Some(&f.admin),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    assert_eq!(f.ask("www.shop.com").await, 403);
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn bad_and_duplicate_domains_are_rejected(pool: PgPool) {
    let f = fixture(pool).await;
    assert_eq!(f.add("shop.com", None).await.status, StatusCode::OK);
    let dup = f.add("SHOP.com.", None).await;
    assert_eq!(dup.status, StatusCode::CONFLICT, "{:?}", dup.json);

    // The platform's own names are never custom domains.
    for host in ["x.apps.test", "apps.test", "cloud.test", "a.cloud.test"] {
        let r = f.add(host, None).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{host}: {:?}", r.json);
    }
    // Wildcards need a DNS-01 provider, which isn't configured.
    let r = f.add("*.shop.com", None).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("DNS provider"),
        "{:?}",
        r.json
    );
    for (host, port) in [("1.2.3.4", None), ("a_b.com", None), ("ok.com", Some(0))] {
        let r = f.add(host, port).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{host}: {:?}", r.json);
    }

    // Apex names get an A record to the serving node.
    let list = f
        .app
        .call(
            Method::GET,
            &format!("/api/service-instances/{}/domains", f.instance),
            Some(&f.admin),
            None,
        )
        .await;
    let apex = &list.json[0]["dnsRecords"][0];
    assert_eq!(apex["type"], "A");
    assert_eq!(apex["value"], "203.0.113.7");
}

#[sqlx::test(migrator = "modula_server::db::MIGRATOR")]
async fn the_background_verifier_routes_a_domain_once_dns_is_in_place(pool: PgPool) {
    let f = fixture(pool).await;
    let r = f.add("app.shop.com", None).await;
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.json);
    let token = r.json["dnsRecords"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["type"] == "TXT")
        .unwrap()["value"]
        .as_str()
        .unwrap()
        .to_owned();
    f.dns.set(
        RecordKind::Txt,
        "_modula-challenge.app.shop.com",
        &[token.as_str()],
    );
    let stop = tokio_util::sync::CancellationToken::new();
    // The loop's first tick runs right away.
    tokio::spawn(modula_server::domains::verify_loop(
        f.app.state.clone(),
        stop.clone(),
    ));
    for _ in 0..100 {
        if f.ask("app.shop.com").await == 200 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    stop.cancel();
    assert_eq!(f.ask("app.shop.com").await, 200);
    let status: String =
        sqlx::query_scalar("SELECT status FROM domains WHERE hostname = 'app.shop.com'")
            .fetch_one(f.app.db())
            .await
            .unwrap();
    assert_eq!(status, "verified");
}
