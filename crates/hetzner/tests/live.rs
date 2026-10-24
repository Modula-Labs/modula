//! Every Hetzner endpoint we call, against the real API. Ignored by default:
//! it needs a read & write token and creates (then deletes) an SSH key, a
//! firewall and the cheapest server, which costs about a cent.
//!
//! ```sh
//! read -rs HCLOUD_TOKEN && export HCLOUD_TOKEN
//! cargo test -p modula-hetzner --test live -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use modula_hetzner::{Client, CreateServer, FirewallRule, ProviderError, price_cents};
use secrecy::SecretString;

/// A throwaway key; its private half was never kept.
const PUBLIC_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOrxwR+WX8aN3EIEXib5i0nDf1vm2r5zz9sfR/idwPYv modula-live-test";
const NAME: &str = "modula-live-test";

fn client() -> Client {
    let token = std::env::var("HCLOUD_TOKEN").expect("set HCLOUD_TOKEN");
    let api = std::env::var("HCLOUD_API").unwrap_or_else(|_| "https://api.hetzner.cloud/v1".into());
    Client::new(api.parse().unwrap(), SecretString::from(token)).unwrap()
}

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([("modula-live-test".to_owned(), "1".to_owned())])
}

fn rule(port: &'static str) -> FirewallRule {
    FirewallRule::inbound_any("tcp", Some(port), "live test")
}

async fn wait(hz: &Client, action: i64, what: &str) {
    for _ in 0..120 {
        let a = hz.action(action).await.expect("GET /actions/{id}");
        match a.status.as_str() {
            "success" => return println!("  ✓ {what}"),
            "error" => panic!("{what} failed: {:?}", a.error),
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    panic!("{what} timed out");
}

/// Leftovers from an earlier, interrupted run.
async fn clean(hz: &Client) {
    for s in hz.servers("modula-live-test").await.unwrap() {
        hz.delete_server(s.id).await.unwrap();
        println!("  cleaned up server {}", s.id);
    }
    for k in hz.ssh_keys().await.unwrap() {
        if k.name == NAME {
            hz.delete_ssh_key(k.id).await.unwrap();
        }
    }
    for f in hz.firewalls("modula-live-test").await.unwrap() {
        // Fails while a just-deleted server still holds it; the next run gets it.
        let _ = hz.delete_firewall(f.id).await;
    }
}

#[tokio::test]
#[ignore = "needs HCLOUD_TOKEN; creates real resources"]
async fn every_endpoint() {
    let hz = client();
    clean(&hz).await;

    println!("reads");
    let locations = hz.locations().await.expect("GET /locations");
    println!("  ✓ GET /locations: {}", locations.len());
    assert!(!locations.is_empty());
    let types = hz.server_types().await.expect("GET /server_types");
    println!("  ✓ GET /server_types: {}", types.len());
    let with_availability = types
        .iter()
        .flat_map(|t| &t.locations)
        .filter(|l| l.available.is_some())
        .count();
    println!("    locations[] entries carrying `available`: {with_availability}");
    hz.ssh_keys().await.expect("GET /ssh_keys");
    println!("  ✓ GET /ssh_keys");
    hz.servers("modula").await.expect("GET /servers");
    println!("  ✓ GET /servers");
    hz.firewalls("modula").await.expect("GET /firewalls");
    println!("  ✓ GET /firewalls");

    // The cheapest x86 type orderable somewhere right now.
    let (ty, loc, cents) = types
        .iter()
        .filter(|t| t.architecture == "x86")
        .flat_map(|t| {
            locations
                .iter()
                .filter(|l| t.orderable_in(&l.name))
                .filter_map(move |l| {
                    Some((t, l, price_cents(&t.monthly_price_in(&l.name)?.gross)?))
                })
        })
        .min_by_key(|(_, _, c)| *c)
        .expect("no orderable x86 type anywhere");
    println!(
        "  cheapest orderable: {} in {} (€{}.{:02}/mo)",
        ty.name,
        loc.name,
        cents / 100,
        cents % 100
    );

    println!("writes");
    let key = hz
        .create_ssh_key(NAME, PUBLIC_KEY, &labels())
        .await
        .expect("POST /ssh_keys");
    println!("  ✓ POST /ssh_keys");
    let fw = hz
        .create_firewall(NAME, &labels(), &[rule("22")])
        .await
        .expect("POST /firewalls");
    println!("  ✓ POST /firewalls");
    hz.set_firewall_rules(fw.id, &[rule("22"), rule("443")])
        .await
        .expect("set_rules");
    println!("  ✓ POST /firewalls/{{id}}/actions/set_rules");

    let req = CreateServer {
        name: NAME.into(),
        server_type: ty.name.clone(),
        location: loc.name.clone(),
        image: "ubuntu-24.04".into(),
        ssh_keys: vec![key.id],
        user_data: "#cloud-config\n".into(),
        labels: labels(),
        firewalls: vec![],
        start_after_create: true,
    };
    let result = async {
        let (server, action) = hz.create_server(&req).await?;
        println!("  ✓ POST /servers ({})", server.id);
        Ok::<_, ProviderError>((server, action))
    }
    .await;
    let (server, action) = match result {
        Ok(v) => v,
        Err(e) => {
            hz.delete_ssh_key(key.id).await.ok();
            hz.delete_firewall(fw.id).await.ok();
            panic!("POST /servers: {e:?}");
        }
    };
    if let Some(a) = action {
        wait(&hz, a, "GET /actions/{id} (create)").await;
    }
    let got = hz.server(server.id).await.expect("GET /servers/{id}");
    println!("  ✓ GET /servers/{{id}}: {} {:?}", got.status, got.ipv4());
    hz.apply_firewall(fw.id, server.id)
        .await
        .expect("apply_to_resources");
    println!("  ✓ POST /firewalls/{{id}}/actions/apply_to_resources");
    let a = hz
        .server_action(server.id, "poweroff")
        .await
        .expect("poweroff");
    println!("  ✓ POST /servers/{{id}}/actions/poweroff");
    wait(&hz, a.id, "poweroff").await;

    println!("cleanup");
    hz.delete_server(server.id)
        .await
        .expect("DELETE /servers/{id}");
    println!("  ✓ DELETE /servers/{{id}}");
    hz.delete_ssh_key(key.id)
        .await
        .expect("DELETE /ssh_keys/{id}");
    println!("  ✓ DELETE /ssh_keys/{{id}}");
    // The firewall stays attached until the server is gone.
    let mut deleted = false;
    for _ in 0..30 {
        match hz.delete_firewall(fw.id).await {
            Ok(()) => {
                deleted = true;
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    assert!(deleted, "DELETE /firewalls/{{id}} kept failing");
    println!("  ✓ DELETE /firewalls/{{id}}");
}
