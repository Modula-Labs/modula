//! Live node connections. Each attached node has one outbound channel; a
//! per-connection pusher sends a fresh desired-state snapshot whenever the node
//! is kicked (and periodically, to repair drift).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use modula_id::Id;
use modula_proto::node::v1 as pb;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;
use tonic::Status;

pub type Outbound = mpsc::Sender<Result<pb::ServerMessage, Status>>;

#[derive(Clone)]
struct Conn {
    id: u64,
    tx: Outbound,
    kick: std::sync::Arc<Notify>,
    cancel: CancellationToken,
}

pub struct Registered {
    pub conn_id: u64,
    pub kick: std::sync::Arc<Notify>,
    pub cancel: CancellationToken,
}

#[derive(Default)]
pub struct Hub {
    conns: Mutex<HashMap<Id, Conn>>,
    /// request id → (node that must answer, where chunks go).
    logs: Mutex<HashMap<String, (Id, mpsc::Sender<pb::LogChunk>)>>,
    next: AtomicU64,
}

impl Hub {
    /// Register a node's stream, replacing (and cancelling) any older one.
    pub fn register(&self, node: Id, tx: Outbound) -> Registered {
        let conn = Conn {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            tx,
            kick: std::sync::Arc::new(Notify::new()),
            cancel: CancellationToken::new(),
        };
        let reg = Registered {
            conn_id: conn.id,
            kick: conn.kick.clone(),
            cancel: conn.cancel.clone(),
        };
        if let Some(old) = self.conns.lock().expect("hub lock").insert(node, conn) {
            old.cancel.cancel();
        }
        reg
    }

    /// Remove the connection if it is still the current one for `node`.
    pub fn unregister(&self, node: Id, conn_id: u64) -> bool {
        let mut conns = self.conns.lock().expect("hub lock");
        if conns.get(&node).is_some_and(|c| c.id == conn_id) {
            if let Some(c) = conns.remove(&node) {
                c.cancel.cancel();
            }
            self.logs
                .lock()
                .expect("hub lock")
                .retain(|_, (n, _)| *n != node);
            return true;
        }
        false
    }

    pub fn disconnect(&self, node: Id) {
        if let Some(c) = self.conns.lock().expect("hub lock").remove(&node) {
            c.cancel.cancel();
        }
        self.logs
            .lock()
            .expect("hub lock")
            .retain(|_, (n, _)| *n != node);
    }

    pub fn is_connected(&self, node: Id) -> bool {
        self.conns.lock().expect("hub lock").contains_key(&node)
    }

    pub fn kick(&self, node: Id) {
        if let Some(c) = self.conns.lock().expect("hub lock").get(&node) {
            c.kick.notify_one();
        }
    }

    pub fn kick_all(&self, nodes: &[Id]) {
        for n in nodes {
            self.kick(*n);
        }
    }

    fn sender(&self, node: Id) -> Option<Outbound> {
        self.conns
            .lock()
            .expect("hub lock")
            .get(&node)
            .map(|c| c.tx.clone())
    }

    /// Ask `node` for a deployment's logs. Chunks arrive on the returned receiver
    /// until one has `done` set or the stream is closed with [`Hub::close_logs`].
    pub async fn open_logs(
        &self,
        node: Id,
        deployment: Id,
        tail: u32,
        follow: bool,
        kind: pb::LogKind,
    ) -> Option<(String, mpsc::Receiver<pb::LogChunk>)> {
        let tx = self.sender(node)?;
        let request_id = Id::new().to_string();
        let (ctx, crx) = mpsc::channel(64);
        self.logs
            .lock()
            .expect("hub lock")
            .insert(request_id.clone(), (node, ctx));
        let msg = pb::ServerMessage {
            msg: Some(pb::server_message::Msg::Logs(pb::LogRequest {
                request_id: request_id.clone(),
                deployment_id: deployment.to_string(),
                tail,
                follow,
                kind: kind as i32,
            })),
        };
        if tx.send(Ok(msg)).await.is_err() {
            self.logs.lock().expect("hub lock").remove(&request_id);
            return None;
        }
        Some((request_id, crx))
    }

    pub async fn close_logs(&self, request_id: &str) {
        let Some((node, _)) = self.logs.lock().expect("hub lock").remove(request_id) else {
            return;
        };
        if let Some(tx) = self.sender(node) {
            let msg = pb::ServerMessage {
                msg: Some(pb::server_message::Msg::LogCancel(pb::LogCancel {
                    request_id: request_id.to_owned(),
                })),
            };
            let _ = tx.send(Ok(msg)).await;
        }
    }

    /// Route a chunk from `node` without waiting. Logs are best effort: if the
    /// viewer is gone or too slow, the request is dropped and its id returned
    /// so the caller can cancel it on the node. Waiting here would stall every
    /// other message from that node (deployment events included) behind one
    /// slow browser. Chunks for requests another node owns are ignored.
    pub fn deliver_logs(&self, node: Id, chunk: pb::LogChunk) -> Option<String> {
        let mut logs = self.logs.lock().expect("hub lock");
        let id = chunk.request_id.clone();
        let tx = match logs.get(&id) {
            Some((owner, tx)) if *owner == node => tx,
            _ => return None,
        };
        let done = chunk.done;
        match tx.try_send(chunk) {
            Ok(()) if done => {
                logs.remove(&id);
                None
            }
            Ok(()) => None,
            Err(_) => {
                logs.remove(&id);
                Some(id)
            }
        }
    }
}
