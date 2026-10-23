//! Real Docker Engine round trip. Ignored by default (needs a Docker daemon):
//!
//!     cargo test -p modula-node --test docker -- --ignored
//!
//! The HTTP health probe is not exercised here because Docker Desktop on macOS
//! cannot route to bridge IPs from the host; it runs in the Linux e2e suite.

use futures::StreamExt;
use modula_node::runtime::Runtime;
use modula_node::runtime::docker::DockerRuntime;
use modula_proto::node::v1 as pb;
use uuid::Uuid;

#[tokio::test]
#[ignore = "needs a Docker daemon"]
async fn container_lifecycle() {
    let rt = DockerRuntime::connect().unwrap();
    assert!(rt.describe().await.unwrap().starts_with("docker "));
    let (env, dep) = (Uuid::now_v7().to_string(), Uuid::now_v7().to_string());
    let w = pb::Workload {
        deployment_id: dep.clone(),
        service_id: Uuid::nil().to_string(),
        environment_id: env.clone(),
        service_name: "smoke".into(),
        image: "busybox:1.36".into(),
        command: vec![
            "sh".into(),
            "-c".into(),
            "echo hello-$GREETING; sleep 30".into(),
        ],
        env: vec![pb::EnvVar {
            name: "GREETING".into(),
            value: "modula".into(),
        }],
        port: 8080,
        limits: Some(pb::Limits {
            memory_bytes: 64 << 20,
            nano_cpus: 250_000_000,
            pids: 64,
        }),
        ..Default::default()
    };

    rt.ensure_network(&env).await.unwrap();
    rt.ensure_network(&env).await.unwrap(); // idempotent
    rt.pull(&w.image).await.unwrap();
    let id = rt.create(&w).await.unwrap();
    rt.start(&id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let mine: Vec<_> = rt
        .containers()
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.deployment_id == dep)
        .collect();
    assert_eq!(mine.len(), 1);
    assert!(mine[0].running);

    let lines: Vec<_> = rt.logs(&id, 10, false).await.unwrap().collect().await;
    let text: Vec<String> = lines.into_iter().map(|l| l.unwrap().text).collect();
    assert!(text.iter().any(|t| t == "hello-modula"), "{text:?}");

    rt.remove(&id).await.unwrap();
    assert!(
        rt.containers()
            .await
            .unwrap()
            .iter()
            .all(|c| c.deployment_id != dep)
    );
    rt.remove_network(&env).await.unwrap();
    assert!(!rt.networks().await.unwrap().contains(&env));
}
