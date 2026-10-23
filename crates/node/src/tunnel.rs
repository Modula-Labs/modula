//! The node's end of private TCP tunnels (`modula connect`).
//!
//! The control plane asks for a tunnel to one deployment's container; the node
//! dials that container on its environment network (its own address from the
//! runtime, never a host named by the server) and pipes bytes over the
//! session stream. Flow control mirrors the server's (see the server's
//! `tunnel` module): at most [`WINDOW`] bytes in flight per direction, credit
//! returned as bytes are written to the container. Tunnels belong to one
//! session and end with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use modula_proto::node::v1 as pb;
use pb::node_message::Msg as Out;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

use crate::runtime::Runtime;

/// Largest data chunk in one `TunnelData`.
pub const CHUNK: usize = 64 * 1024;
/// Bytes a sender may have in flight per tunnel and direction.
pub const WINDOW: usize = 256 * 1024;
const CREDIT_EVERY: usize = WINDOW / 4;
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_TUNNELS: usize = 64;

/// A byte window: `take` waits for room, `give` returns credit (capped at
/// what is outstanding).
struct Credit {
    room: Semaphore,
    in_flight: AtomicUsize,
}

impl Credit {
    fn new() -> Self {
        Self {
            room: Semaphore::new(WINDOW),
            in_flight: AtomicUsize::new(0),
        }
    }

    async fn take(&self, n: usize) {
        if let Ok(p) = self.room.acquire_many(n as u32).await {
            p.forget();
            self.in_flight.fetch_add(n, Ordering::AcqRel);
        }
    }

    fn give(&self, n: usize) {
        let given = self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |f| Some(f - n.min(f)))
            .map_or(0, |f| n.min(f));
        self.room.add_permits(given);
    }
}

struct Tunnel {
    /// Bytes from the server, to be written to the container.
    data: mpsc::UnboundedSender<Vec<u8>>,
    /// Credit for our sends to the server.
    credit: Arc<Credit>,
    /// Received but not yet written (≤ WINDOW).
    buffered: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

type Map = Arc<Mutex<HashMap<String, Tunnel>>>;

/// This session's open tunnels. Dropping it closes them all.
pub struct Tunnels {
    runtime: Arc<dyn Runtime>,
    tx: mpsc::Sender<pb::NodeMessage>,
    open: Map,
}

fn msg(m: Out) -> pb::NodeMessage {
    pb::NodeMessage { msg: Some(m) }
}

fn close_msg(tunnel_id: String, reason: impl Into<String>) -> pb::NodeMessage {
    msg(Out::TunnelClose(pb::TunnelClose {
        tunnel_id,
        reason: reason.into(),
    }))
}

impl Tunnels {
    pub fn new(runtime: Arc<dyn Runtime>, tx: mpsc::Sender<pb::NodeMessage>) -> Self {
        Self {
            runtime,
            tx,
            open: Arc::default(),
        }
    }

    /// Start dialling; the answer (`TunnelOpened`) is sent from a task so the
    /// session loop never waits on a container.
    pub fn open(&self, req: pb::TunnelOpen) {
        let mut open = self.open.lock().expect("tunnels lock");
        if open.len() >= MAX_TUNNELS || open.contains_key(&req.tunnel_id) {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx
                    .send(msg(Out::TunnelOpened(pb::TunnelOpened {
                        tunnel_id: req.tunnel_id,
                        ok: false,
                        error: "too many tunnels are open on this server".into(),
                    })))
                    .await;
            });
            return;
        }
        let (data_tx, data_rx) = mpsc::unbounded_channel();
        let credit = Arc::new(Credit::new());
        let buffered = Arc::new(AtomicUsize::new(0));
        let id = req.tunnel_id.clone();
        let task = tokio::spawn(run(
            req,
            self.runtime.clone(),
            self.tx.clone(),
            data_rx,
            credit.clone(),
            buffered.clone(),
            self.open.clone(),
        ));
        open.insert(
            id,
            Tunnel {
                data: data_tx,
                credit,
                buffered,
                task,
            },
        );
    }

    pub fn data(&self, m: pb::TunnelData) {
        let mut open = self.open.lock().expect("tunnels lock");
        let Some(t) = open.get(&m.tunnel_id) else {
            return;
        };
        if m.credit > 0 {
            t.credit.give(m.credit as usize);
        }
        if m.data.is_empty() {
            return;
        }
        let len = m.data.len();
        let over = len > CHUNK || t.buffered.fetch_add(len, Ordering::AcqRel) + len > WINDOW;
        if over || t.data.send(m.data).is_err() {
            if let Some(t) = open.remove(&m.tunnel_id) {
                t.task.abort();
            }
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let _ = tx
                    .send(close_msg(m.tunnel_id, "flow control violated"))
                    .await;
            });
        }
    }

    /// The server closed the tunnel.
    pub fn close(&self, m: pb::TunnelClose) {
        if let Some(t) = self.open.lock().expect("tunnels lock").remove(&m.tunnel_id) {
            t.task.abort();
        }
    }
}

impl Drop for Tunnels {
    fn drop(&mut self) {
        for (_, t) in self.open.lock().expect("tunnels lock").drain() {
            t.task.abort();
        }
    }
}

/// Dial the deployment's container, then pump bytes both ways until either
/// side closes.
async fn run(
    req: pb::TunnelOpen,
    runtime: Arc<dyn Runtime>,
    tx: mpsc::Sender<pb::NodeMessage>,
    data: mpsc::UnboundedReceiver<Vec<u8>>,
    credit: Arc<Credit>,
    buffered: Arc<AtomicUsize>,
    open: Map,
) {
    let id = req.tunnel_id.clone();
    let reason = match dial(&runtime, &req).await {
        Ok(stream) => {
            let opened = msg(Out::TunnelOpened(pb::TunnelOpened {
                tunnel_id: id.clone(),
                ok: true,
                error: String::new(),
            }));
            if tx.send(opened).await.is_err() {
                return;
            }
            tracing::info!(tunnel = %id, deployment = %req.deployment_id, port = req.port, "tunnel opened");
            pump(&id, stream, &tx, data, &credit, &buffered).await
        }
        Err(e) => {
            open.lock().expect("tunnels lock").remove(&id);
            let _ = tx
                .send(msg(Out::TunnelOpened(pb::TunnelOpened {
                    tunnel_id: id,
                    ok: false,
                    error: format!("{e:#}"),
                })))
                .await;
            return;
        }
    };
    tracing::info!(tunnel = %id, %reason, "tunnel closed");
    open.lock().expect("tunnels lock").remove(&id);
    let _ = tx.send(close_msg(id, reason)).await;
}

async fn dial(runtime: &Arc<dyn Runtime>, req: &pb::TunnelOpen) -> anyhow::Result<TcpStream> {
    let port = u16::try_from(req.port)
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| anyhow::anyhow!("invalid port {}", req.port))?;
    let container = runtime
        .containers()
        .await?
        .into_iter()
        .find(|c| c.deployment_id == req.deployment_id && c.running)
        .ok_or_else(|| anyhow::anyhow!("the deployment is not running on this server"))?;
    let ip = runtime
        .address(&container.id, &container.environment_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the container has no network address yet"))?;
    let stream = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect((ip, port)))
        .await
        .map_err(|_| anyhow::anyhow!("nothing answered on port {port} in time"))?
        .map_err(|e| anyhow::anyhow!("nothing is listening on port {port} ({e})"))?;
    let _ = stream.set_nodelay(true);
    Ok(stream)
}

async fn pump(
    id: &str,
    stream: TcpStream,
    tx: &mpsc::Sender<pb::NodeMessage>,
    mut data: mpsc::UnboundedReceiver<Vec<u8>>,
    credit: &Credit,
    buffered: &AtomicUsize,
) -> String {
    let (mut read, mut write) = stream.into_split();
    // Server → container, returning credit as bytes are written.
    let to_container = async {
        let mut owed = 0usize;
        while let Some(bytes) = data.recv().await {
            if let Err(e) = write.write_all(&bytes).await {
                return format!("write to the service failed: {e}");
            }
            buffered.fetch_sub(bytes.len(), Ordering::AcqRel);
            owed += bytes.len();
            if owed >= CREDIT_EVERY {
                let m = msg(Out::TunnelData(pb::TunnelData {
                    tunnel_id: id.to_owned(),
                    data: Vec::new(),
                    credit: std::mem::take(&mut owed) as u32,
                }));
                if tx.send(m).await.is_err() {
                    return "the control plane went away".to_owned();
                }
            }
        }
        "closed".to_owned()
    };
    // Container → server, waiting for credit.
    let from_container = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match read.read(&mut buf).await {
                Ok(0) => return "the service closed the connection".to_owned(),
                Ok(n) => n,
                Err(e) => return format!("read from the service failed: {e}"),
            };
            credit.take(n).await;
            let m = msg(Out::TunnelData(pb::TunnelData {
                tunnel_id: id.to_owned(),
                data: buf[..n].to_vec(),
                credit: 0,
            }));
            if tx.send(m).await.is_err() {
                return "the control plane went away".to_owned();
            }
        }
    };
    tokio::select! {
        r = to_container => r,
        r = from_container => r,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn credit_is_capped_at_what_is_outstanding() {
        let c = Credit::new();
        c.take(100).await;
        c.give(1_000_000);
        assert_eq!(c.room.available_permits(), WINDOW);
    }
}
