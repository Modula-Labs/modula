//! One run of a cron service (`CronRun`): the workload once, to completion,
//! in a one-off container ([`Runtime::run_once`]). Its output goes to the
//! server as `LogBatch` lines of the workload's deployment; the outcome as
//! `CronResult`. Both through the outbox, so they survive reconnects.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use modula_proto::node::v1 as pb;
use pb::node_message::Msg as Out;
use tokio::sync::mpsc;

use crate::build::LOCAL_PREFIX;
use crate::runtime::Runtime;

/// The server sends at most a day; anything larger is cut to this.
const MAX_TIMEOUT: Duration = Duration::from_secs(24 * 3600);
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Lines held for the server; later ones are counted as dropped.
const BUFFER: usize = 50_000;
const MAX_LINE: usize = 8 * 1024;
/// Under the server's limit per batch.
const PER_BATCH: usize = 5_000;

fn msg(m: Out) -> pb::NodeMessage {
    pb::NodeMessage { msg: Some(m) }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[derive(Default)]
struct Lines {
    lines: Vec<pb::ShippedLine>,
    dropped: u32,
}

pub async fn run(
    runtime: Arc<dyn Runtime>,
    mesh: Option<Arc<crate::mesh::Mesh>>,
    req: pb::CronRun,
    out: mpsc::Sender<pb::NodeMessage>,
) {
    let run_id = req.run_id.clone();
    tracing::info!(run = %run_id, "cron run starting");
    let result = execute(runtime, mesh, req, &out).await;
    tracing::info!(run = %run_id, exit_code = result.exit_code, timed_out = result.timed_out, error = %result.error, "cron run finished");
    let _ = out.send(msg(Out::CronResult(result))).await;
}

async fn execute(
    runtime: Arc<dyn Runtime>,
    mesh: Option<Arc<crate::mesh::Mesh>>,
    req: pb::CronRun,
    out: &mpsc::Sender<pb::NodeMessage>,
) -> pb::CronResult {
    let failed = |error: String| pb::CronResult {
        run_id: req.run_id.clone(),
        error,
        ..Default::default()
    };
    let Some(w) = req.workload.as_deref().cloned() else {
        return failed("no workload to run".into());
    };
    if !crate::reconcile::workload_is_sane(&w) {
        return failed("malformed workload".into());
    }
    // Nothing of a cron service runs between runs, so its network may be gone.
    if let Err(e) = runtime.ensure_network(&w.environment_id).await {
        return failed(format!("Could not create the network: {e:#}"));
    }
    if !w.image.starts_with(LOCAL_PREFIX)
        && let Err(e) = runtime.pull(&w.image).await
    {
        return failed(format!("Image pull failed: {e:#}"));
    }
    // A leased mesh address, so it reaches services on other servers.
    let lease = match &mesh {
        Some(m) if !w.mesh_ip.is_empty() => m.lease(&w.environment_id).await,
        _ => None,
    };
    let mut once = w.clone();
    once.mesh_ip = lease.as_ref().map(|l| l.ip.to_string()).unwrap_or_default();

    let buffer = Arc::new(Mutex::new(Lines::default()));
    let sink = {
        let (buffer, deployment) = (buffer.clone(), w.deployment_id.clone());
        Box::new(move |line: crate::runtime::LogLine| {
            let mut b = buffer.lock().expect("cron lines lock");
            if b.lines.len() >= BUFFER {
                b.dropped = b.dropped.saturating_add(1);
                return;
            }
            let mut text = line.text;
            if text.len() > MAX_LINE {
                let mut end = MAX_LINE;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            b.lines.push(pb::ShippedLine {
                deployment_id: deployment.clone(),
                at_unix_ms: if line.ts_unix_ms > 0 {
                    line.ts_unix_ms
                } else {
                    now_ms()
                },
                stderr: line.stderr,
                text,
            });
        })
    };
    let flusher = {
        let (buffer, out) = (buffer.clone(), out.clone());
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(FLUSH_EVERY).await;
                flush(&buffer, &out).await;
            }
        })
    };
    let timeout = Duration::from_secs(u64::from(req.timeout_secs.max(1))).min(MAX_TIMEOUT);
    let started = Instant::now();
    let result = runtime.run_once(&once, &w.command, timeout, sink).await;
    flusher.abort();
    flush(&buffer, out).await;
    drop(lease);
    match result {
        Ok(code) => pb::CronResult {
            run_id: req.run_id.clone(),
            exit_code: code,
            ..Default::default()
        },
        Err(_) if started.elapsed() >= timeout => pb::CronResult {
            run_id: req.run_id.clone(),
            timed_out: true,
            error: format!("Stopped after {}s (the cron timeout)", timeout.as_secs()),
            ..Default::default()
        },
        Err(e) => failed(format!("{e:#}")),
    }
}

/// Everything buffered, in batches; waits for room in the outbox.
async fn flush(buffer: &Mutex<Lines>, out: &mpsc::Sender<pb::NodeMessage>) {
    loop {
        let batch = {
            let mut b = buffer.lock().expect("cron lines lock");
            if b.lines.is_empty() && b.dropped == 0 {
                return;
            }
            let n = b.lines.len().min(PER_BATCH);
            pb::LogBatch {
                lines: b.lines.drain(..n).collect(),
                dropped: std::mem::take(&mut b.dropped),
            }
        };
        if out.send(msg(Out::LogBatch(batch))).await.is_err() {
            return;
        }
    }
}
