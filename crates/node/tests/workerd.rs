//! Functions in the real, pinned workerd (`scripts/fetch-build-tools.sh`
//! fetches it; on a Mac, the same release's darwin build):
//!
//!   MODULA_TEST_WORKERD=/path/to/workerd cargo test -p modula-node --test workerd -- --ignored

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use modula_id::Id;
use modula_node::functions::{FunctionSpec, Functions, Workerd};
use tokio_util::sync::CancellationToken;

const HELLO: &str = r#"
export default {
  async fetch(request, env) {
    console.log("hello from", new URL(request.url).pathname);
    let metadata = "reached";
    try { await fetch("http://169.254.169.254/latest/meta-data/"); } catch { metadata = "blocked"; }
    let local = "reached";
    try { await fetch("http://127.0.0.1:7102/"); } catch { local = "blocked"; }
    return Response.json({ greeting: env.GREETING, metadata, local });
  },
};
"#;

#[tokio::test]
#[ignore = "needs the workerd binary in MODULA_TEST_WORKERD"]
async fn a_hello_function_serves_in_real_workerd() {
    let bin = std::env::var("MODULA_TEST_WORKERD").expect("MODULA_TEST_WORKERD");
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = std::env::temp_dir().join(format!("modula-workerd-{}", Id::new()));
    let launcher = Arc::new(Workerd::new(bin.into()).await);
    let f = Functions::new(dir.clone(), launcher).unwrap();
    let (out, mut shipped) = tokio::sync::mpsc::channel(64);
    let stop = CancellationToken::new();
    tokio::spawn(f.clone().run(out, stop.clone()));

    let dep = Id::new().to_string();
    let spec = FunctionSpec {
        deployment_id: dep.clone(),
        compatibility_date: "2026-10-01".into(),
        code: HELLO.into(),
        bindings: vec![("GREETING".into(), "a \"quoted\" \\ héllo\nthere".into())],
    };
    f.check(&spec).await.unwrap();
    let v = f.upsert(spec).unwrap();
    f.wait_serving(v, Duration::from_secs(30)).await.unwrap();

    let port = f.port(&dep).unwrap();
    let body: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/hi"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["greeting"], "a \"quoted\" \\ héllo\nthere");
    assert_eq!(body["metadata"], "blocked", "no metadata service");
    assert_eq!(body["local"], "blocked", "nothing on this server");

    // console.log reaches the deployment's logs, tagged and shipped.
    let mut logs = f.logs(&dep, 10, true).unwrap();
    let line = tokio::time::timeout(Duration::from_secs(5), logs.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(line.text, "hello from /hi");
    let batch = tokio::time::timeout(Duration::from_secs(5), shipped.recv())
        .await
        .unwrap()
        .unwrap();
    match batch.msg {
        Some(modula_proto::node::v1::node_message::Msg::LogBatch(b)) => {
            assert!(
                b.lines
                    .iter()
                    .any(|l| l.deployment_id == dep && l.text == "hello from /hi")
            );
        }
        other => panic!("unexpected {other:?}"),
    }

    // Code that doesn't load is refused with workerd's own message, and the
    // serving function is untouched.
    let broken = FunctionSpec {
        deployment_id: Id::new().to_string(),
        compatibility_date: "2026-10-01".into(),
        code: "export default { fetch( } ".into(),
        bindings: vec![],
    };
    let err = f.check(&broken).await.unwrap_err();
    assert!(err.contains("failed to start"), "{err}");
    assert!(
        reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .is_ok()
    );

    f.retain(&HashSet::new());
    stop.cancel();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let _ = std::fs::remove_dir_all(&dir);
}
