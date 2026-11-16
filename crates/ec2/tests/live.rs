//! EC2 end to end against real AWS. Ignored by default: it creates billable
//! resources (the cheapest instance, an Elastic IP, security groups) for a
//! few minutes and removes them, even on failure.
//!
//! ```sh
//! read -rs AWS_SECRET_ACCESS_KEY && export AWS_SECRET_ACCESS_KEY AWS_ACCESS_KEY_ID=...
//! cargo test -p modula-ec2 --test live -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use modula_ec2::provider::Ec2Provider;
use modula_ec2::{Client, Credentials};
use modula_provider::{MeshSources, NewServer, Phase, ServerProvider};
use secrecy::SecretString;

const REGION: &str = "us-east-1";
const TYPE: &str = "t4g.nano";

fn provider() -> Ec2Provider {
    let creds = Credentials {
        access_key_id: std::env::var("AWS_ACCESS_KEY_ID").expect("set AWS_ACCESS_KEY_ID"),
        secret_access_key: SecretString::from(
            std::env::var("AWS_SECRET_ACCESS_KEY").expect("set AWS_SECRET_ACCESS_KEY"),
        ),
    };
    Ec2Provider::new(
        Client::new(None, creds).unwrap(),
        "modula-live-test",
        Vec::new(),
    )
}

#[tokio::test]
#[ignore = "creates real, billable AWS resources"]
async fn every_call_against_real_aws() {
    let p = provider();
    assert!(!p.locations().await.unwrap().is_empty());
    let offers = p.server_types_at("aws:us-east-1").await.unwrap();
    assert!(offers.iter().any(|t| t.name == TYPE));

    let modula_id = format!("live-{}", time::OffsetDateTime::now_utc().unix_timestamp());
    let req = NewServer {
        name: "modula-live-test".into(),
        region: REGION.into(),
        server_type: TYPE.into(),
        user_data: "#!/bin/bash\necho ok\n".into(),
        labels: BTreeMap::from([
            ("modula".to_owned(), "1".to_owned()),
            ("workspace".to_owned(), "modula-live-test".to_owned()),
            ("modula-id".to_owned(), modula_id.clone()),
        ]),
        ssh_key_ids: Vec::new(),
        ssh: false,
        disk_gb: Some(20),
    };
    let created = p.create(&req).await.unwrap();
    println!("created {}", created.id);

    let result = exercise(&p, &created.id).await;

    // Always clean up: terminate, release the address.
    p.delete(&created.id).await.expect("delete");
    println!("deleted");
    result.unwrap();
}

async fn exercise(p: &Ec2Provider, id: &str) -> Result<(), String> {
    let mut ipv4 = None;
    for _ in 0..60 {
        let s = p.status(id, None).await.map_err(|e| e.to_string())?;
        if s.phase == Phase::Running && s.ipv4.is_some() {
            ipv4 = s.ipv4;
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    let ipv4 = ipv4.ok_or("never reached running with its address")?;
    println!("running at {ipv4}");
    let found = p
        .find_by_address(&ipv4)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("find_by_address found nothing")?;
    if found.disk_gb != 20 {
        return Err(format!(
            "expected a 20 GB root disk, found {}",
            found.disk_gb
        ));
    }
    p.sync_mesh(&MeshSources {
        workspace: vec!["203.0.113.1".into()],
        everyone: Vec::new(),
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}
