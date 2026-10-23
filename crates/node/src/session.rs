//! The long-lived connection to the control plane.
//!
//! One outbound mTLS stream carries everything: the server sends desired state,
//! log requests and renewed certificates; the node sends Hello, deployment
//! events, state reports, log chunks and heartbeats. The reconciler runs
//! independently, so workloads keep running (and restarting) while the server
//! is unreachable; reconnects use jittered exponential backoff.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use futures::StreamExt;
use modula_proto::node::v1 as pb;
use pb::node_client::NodeClient;
use pb::node_message::Msg as Out;
use pb::server_message::Msg as In;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::build::BuildLogs;
use crate::identity::{Identity, pem, pem_blocks};
use crate::join::{connect, renewal_request};
use crate::reconcile::{Capabilities, Reconciler};
use crate::runtime::Runtime;

const HEARTBEAT_EVERY: Duration = Duration::from_secs(15);
/// Kept short so a server upgrade or restart costs nodes seconds, not minutes;
/// a failed attempt is one TCP connect.
const MAX_BACKOFF: Duration = Duration::from_secs(8);
/// A session that lasted this long resets the backoff.
const STABLE_AFTER: Duration = Duration::from_secs(60);
/// Renew when less than this much certificate lifetime is left.
const RENEW_BEFORE: Duration = Duration::from_secs(10 * 24 * 3600);
const LOG_BATCH_LINES: usize = 200;
const LOG_BATCH_WAIT: Duration = Duration::from_millis(50);

/// Run until `stop` is cancelled.
pub async fn run(
    dir: PathBuf,
    runtime: Arc<dyn Runtime>,
    caps: Capabilities,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    let (out_tx, mut out_rx) = mpsc::channel(1024);
    let has_edge = caps.edge.is_some();
    let build_logs = caps.build.as_ref().map(|b| b.logs.clone());
    let db = caps.db.clone();
    // Backup results also go through the outbox, to survive reconnects.
    let results = out_tx.clone();
    let reconciler = Reconciler::new(runtime.clone(), caps, out_tx);
    let (desired_tx, desired_rx) = watch::channel(None);
    let reconcile_task = tokio::spawn(reconciler.clone().run(desired_rx, stop.clone()));
    // At most ~2 s, once per process: the location label for the dashboard.
    let location = crate::location::detect().await;
    tracing::info!(provider = %location.provider, region = %location.region, edge = %location.colo, "location hints");

    let mut backoff = Duration::from_secs(1);
    while !stop.is_cancelled() {
        let identity = Identity::load(&dir)?;
        let started = Instant::now();
        let mut ctx = Ctx {
            identity: &identity,
            runtime: &runtime,
            reconciler: &reconciler,
            desired: &desired_tx,
            outbox: &mut out_rx,
            stop: &stop,
            has_edge,
            build_logs: build_logs.as_ref(),
            results: &results,
            location: &location,
            db: db.as_ref(),
        };
        match ctx.session().await {
            Ok(()) => break,
            Err(e) => tracing::warn!(
                error = format!("{e:#}"),
                "disconnected from the control plane"
            ),
        }
        if started.elapsed() > STABLE_AFTER {
            backoff = Duration::from_secs(1);
        }
        let jitter = Duration::from_millis(u64::from(modula_id::Id::new().as_bytes()[0]) * 4);
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tokio::time::sleep(backoff + jitter) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
    let _ = reconcile_task.await;
    Ok(())
}

struct Ctx<'a> {
    identity: &'a Identity,
    runtime: &'a Arc<dyn Runtime>,
    reconciler: &'a Arc<Reconciler>,
    desired: &'a watch::Sender<Option<pb::DesiredState>>,
    outbox: &'a mut mpsc::Receiver<pb::NodeMessage>,
    stop: &'a CancellationToken,
    has_edge: bool,
    results: &'a mpsc::Sender<pb::NodeMessage>,
    build_logs: Option<&'a BuildLogs>,
    location: &'a crate::location::Hints,
    /// Runs Database-tab operations; None = this node doesn't offer them.
    db: Option<&'a Arc<dyn crate::db::Executor>>,
}

fn msg(m: Out) -> pb::NodeMessage {
    pb::NodeMessage { msg: Some(m) }
}

impl Ctx<'_> {
    async fn session(&mut self) -> anyhow::Result<()> {
        let meta = &self.identity.meta;
        let channel = connect(
            &meta.server,
            self.identity.fingerprint()?,
            Some(self.identity),
        )
        .await?;
        let (tx, rx) = mpsc::channel::<pb::NodeMessage>(256);
        tx.send(msg(Out::Hello(self.hello().await))).await?;
        let mut inbound = NodeClient::new(channel)
            .attach(ReceiverStream::new(rx))
            .await
            .map_err(|s| anyhow::anyhow!("the server rejected this node: {}", s.message()))?
            .into_inner();
        tracing::info!(server = %meta.server, node = %meta.node_id, "connected");

        for m in self.reconciler.replay() {
            tx.send(m).await?;
        }
        let mut pending_key = None;
        if self.needs_renewal() {
            let (key, csr_der) = renewal_request()?;
            pending_key = Some(key);
            tx.send(msg(Out::Renew(pb::RenewRequest { csr_der })))
                .await?;
            tracing::info!("requested a certificate renewal");
        }

        let mut logs: HashMap<String, JoinHandle<()>> = HashMap::new();
        // Closed (dropped) when this session ends.
        let tunnels = crate::tunnel::Tunnels::new(self.runtime.clone(), tx.clone());
        let mut heartbeat = tokio::time::interval(HEARTBEAT_EVERY);
        let result = loop {
            tokio::select! {
                _ = self.stop.cancelled() => break Ok(()),
                incoming = inbound.message() => match incoming {
                    Ok(Some(pb::ServerMessage { msg: Some(m) })) => {
                        if let Err(e) = self.handle(m, &tx, &mut logs, &tunnels, &mut pending_key).await {
                            break Err(e);
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break Err(anyhow::anyhow!("the server closed the stream")),
                    Err(s) => break Err(anyhow::anyhow!("stream error: {}", s.message())),
                },
                Some(m) = self.outbox.recv() => {
                    if tx.send(m).await.is_err() {
                        break Err(anyhow::anyhow!("the stream closed"));
                    }
                }
                _ = heartbeat.tick() => {
                    let at_unix_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis() as i64);
                    let _ = tx.send(msg(Out::Heartbeat(pb::Heartbeat { at_unix_ms }))).await;
                }
            }
            logs.retain(|_, h| !h.is_finished());
        };
        for h in logs.values() {
            h.abort();
        }
        result
    }

    async fn handle(
        &mut self,
        m: In,
        tx: &mpsc::Sender<pb::NodeMessage>,
        logs: &mut HashMap<String, JoinHandle<()>>,
        tunnels: &crate::tunnel::Tunnels,
        pending_key: &mut Option<rcgen::KeyPair>,
    ) -> anyhow::Result<()> {
        match m {
            In::HelloAck(ack) => {
                if ack.node_id != self.identity.meta.node_id.to_string() {
                    bail!("the server identified this node as {}", ack.node_id);
                }
            }
            In::Desired(ds) => {
                let newer = self
                    .desired
                    .borrow()
                    .as_ref()
                    .is_none_or(|cur| ds.generation >= cur.generation);
                if newer {
                    self.desired.send_replace(Some(ds));
                }
            }
            In::Logs(req) => {
                let task = match pb::LogKind::try_from(req.kind) {
                    Ok(pb::LogKind::LogBuild) => tokio::spawn(stream_build_logs(
                        self.build_logs.cloned(),
                        req.clone(),
                        tx.clone(),
                    )),
                    _ => tokio::spawn(stream_logs(self.runtime.clone(), req.clone(), tx.clone())),
                };
                if let Some(old) = logs.insert(req.request_id, task) {
                    old.abort();
                }
            }
            // Not tied to this session: a restore must not stop halfway because
            // the control plane restarted.
            In::Backup(req) => {
                tokio::spawn(crate::backup::backup(
                    self.runtime.clone(),
                    req,
                    self.results.clone(),
                ));
            }
            In::Restore(req) => {
                tokio::spawn(crate::backup::restore(
                    self.runtime.clone(),
                    req,
                    self.results.clone(),
                ));
            }
            // Answered on this session: the caller waits on this connection.
            In::Db(req) => match self.db {
                Some(executor) => {
                    // Credentials as of the newest desired state.
                    let env = self.desired.borrow().as_ref().and_then(|ds| {
                        ds.workloads
                            .iter()
                            .find(|w| w.deployment_id == req.deployment_id)
                            .map(|w| {
                                w.env
                                    .iter()
                                    .map(|e| (e.name.clone(), e.value.clone()))
                                    .collect()
                            })
                    });
                    tokio::spawn(crate::db::handle(
                        self.runtime.clone(),
                        executor.clone(),
                        env,
                        req,
                        tx.clone(),
                    ));
                }
                None => {
                    let r = pb::DbResponse {
                        request_id: req.request_id,
                        error: "this server does not run database operations".into(),
                        ..Default::default()
                    };
                    tx.send(msg(Out::DbResult(r))).await?;
                }
            },
            In::TunnelOpen(req) => tunnels.open(req),
            In::TunnelData(d) => tunnels.data(d),
            In::TunnelClose(c) => tunnels.close(c),
            In::LogCancel(c) => {
                if let Some(h) = logs.remove(&c.request_id) {
                    h.abort();
                }
            }
            In::Renewed(r) => {
                let Some(key) = pending_key.take() else {
                    tracing::warn!("ignoring an unrequested certificate");
                    return Ok(());
                };
                let intermediate = pem_blocks(&self.identity.cert_pem, "CERTIFICATE")
                    .into_iter()
                    .nth(1)
                    .context("node.crt lacks the intermediate certificate")?;
                let mut renewed = self.identity.clone();
                renewed.cert_pem = format!(
                    "{}{}",
                    pem("CERTIFICATE", &r.cert_der),
                    pem("CERTIFICATE", &intermediate)
                );
                renewed.key_pem = key.serialize_pem();
                renewed.save()?;
                tracing::info!("certificate renewed; used from the next connection");
            }
        }
        Ok(())
    }

    fn needs_renewal(&self) -> bool {
        match self.identity.cert_not_after() {
            Ok(t) => t
                .duration_since(std::time::SystemTime::now())
                .map_or(true, |left| left < RENEW_BEFORE),
            Err(_) => false,
        }
    }

    async fn hello(&self) -> pb::Hello {
        pb::Hello {
            protocol: pb::PROTOCOL,
            node_version: crate::VERSION.to_owned(),
            capabilities: ["logs", "renew", "backup", "tunnel"]
                .into_iter()
                .chain(self.has_edge.then_some("edge"))
                .chain(self.build_logs.is_some().then_some("build"))
                .chain(self.db.is_some().then_some("db"))
                .map(String::from)
                .collect(),
            applied_generation: self.reconciler.applied_generation(),
            info: Some(pb::NodeInfo {
                cloud_provider: self.location.provider.clone(),
                cloud_region: self.location.region.clone(),
                edge_colo: self.location.colo.clone(),
                public_ip: self.location.public_ip.clone(),
                ..host_info(self.runtime.describe().await.unwrap_or_default())
            }),
        }
    }
}

fn host_info(runtime_version: String) -> pb::NodeInfo {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_owned())
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default();
    // MemTotal from /proc/meminfo (Linux); 0 elsewhere.
    let memory_bytes = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find_map(|l| l.strip_prefix("MemTotal:"))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        })
        .map_or(0, |kb| kb * 1024);
    pb::NodeInfo {
        hostname,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        cpus: std::thread::available_parallelism().map_or(0, |n| n.get() as u32),
        memory_bytes,
        runtime_version,
        ..Default::default()
    }
}

/// Stream a deployment's build log back, following it while the build runs.
async fn stream_build_logs(
    logs: Option<BuildLogs>,
    req: pb::LogRequest,
    tx: mpsc::Sender<pb::NodeMessage>,
) {
    let chunk = |lines: Vec<pb::LogLine>, done: bool, error: String| {
        msg(Out::Log(pb::LogChunk {
            request_id: req.request_id.clone(),
            lines,
            done,
            error,
        }))
    };
    let reader = logs.and_then(|l| l.read(&req.deployment_id, req.tail, req.follow));
    let Some(mut reader) = reader else {
        let _ = tx
            .send(chunk(vec![], true, "no build log on this server".into()))
            .await;
        return;
    };
    while let Some(batch) = reader.next_batch().await {
        for part in batch.chunks(LOG_BATCH_LINES) {
            let lines = part
                .iter()
                .map(|l| pb::LogLine {
                    ts_unix_ms: l.ts_unix_ms,
                    stderr: l.stderr,
                    text: l.text.clone(),
                })
                .collect();
            if tx.send(chunk(lines, false, String::new())).await.is_err() {
                return;
            }
        }
    }
    let _ = tx.send(chunk(vec![], true, String::new())).await;
}

/// Stream a deployment's logs back in small batches (≤200 lines or 50 ms).
async fn stream_logs(
    runtime: Arc<dyn Runtime>,
    req: pb::LogRequest,
    tx: mpsc::Sender<pb::NodeMessage>,
) {
    let chunk = |lines: Vec<pb::LogLine>, done: bool, error: String| {
        msg(Out::Log(pb::LogChunk {
            request_id: req.request_id.clone(),
            lines,
            done,
            error,
        }))
    };
    let container = match runtime.containers().await {
        Ok(cs) => cs
            .into_iter()
            .find(|c| c.deployment_id == req.deployment_id),
        Err(e) => {
            let _ = tx.send(chunk(vec![], true, format!("{e:#}"))).await;
            return;
        }
    };
    let Some(container) = container else {
        let _ = tx
            .send(chunk(
                vec![],
                true,
                "this deployment has no container on the server".into(),
            ))
            .await;
        return;
    };
    let mut stream = match runtime.logs(&container.id, req.tail, req.follow).await {
        Ok(s) => s,
        Err(e) => {
            let _ = tx.send(chunk(vec![], true, format!("{e:#}"))).await;
            return;
        }
    };
    let mut batch = Vec::new();
    loop {
        // Either a line arrives, the stream ends, or the batch window elapses.
        let (finished, flush) = match tokio::time::timeout(LOG_BATCH_WAIT, stream.next()).await {
            Ok(Some(Ok(l))) => {
                batch.push(pb::LogLine {
                    ts_unix_ms: l.ts_unix_ms,
                    stderr: l.stderr,
                    text: l.text,
                });
                (false, batch.len() >= LOG_BATCH_LINES)
            }
            Ok(Some(Err(e))) => {
                let _ = tx
                    .send(chunk(std::mem::take(&mut batch), true, format!("{e:#}")))
                    .await;
                return;
            }
            Ok(None) => (true, true),
            Err(_) => (false, !batch.is_empty()),
        };
        if flush
            && (tx
                .send(chunk(std::mem::take(&mut batch), finished, String::new()))
                .await
                .is_err()
                || finished)
        {
            return;
        }
    }
}
