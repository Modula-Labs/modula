//! Copying a volume's data from one node to another, relayed over both
//! nodes' existing streams (no new listener, nothing node to node).
//!
//! `VolumeImport` goes to the destination and `VolumeExport` to the source;
//! the source's `VolumeData` is forwarded to the destination and the
//! destination's credit back to the source, so flow control (the tunnels'
//! [`WINDOW`]) is end to end. The relay itself never waits in the stream
//! reader: messages queue on one task per transfer, and a source that has
//! more than a window outstanding has the transfer closed, so the queue stays
//! bounded. The destination's `VolumeClose` ends the transfer.
//!
//! Copies run as tasks of this process, not inside a job (a big volume can
//! take longer than a job's lease): [`Transfers::start_copy`] starts one per
//! instance move and [`Transfers::copy_result`] is polled. After a restart
//! nothing is running, and the move starts the copy over (the destination
//! empties the volume first).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use modula_id::Id;
use modula_proto::node::v1 as pb;
use modula_proto::node::v1::node_message::Msg as In;
use modula_proto::node::v1::server_message::Msg as Out;
use tokio::sync::{mpsc, oneshot};

use crate::state::AppState;
use crate::tunnel::{CHUNK, WINDOW};

/// No data and no credit for this long gives a transfer up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

struct Open {
    source: Id,
    dest: Id,
    relay: mpsc::UnboundedSender<(Id, Out)>,
    /// Bytes the source sent that the destination hasn't credited yet.
    in_flight: Arc<AtomicUsize>,
    /// Unix ms of the last data or credit.
    active: Arc<AtomicU64>,
    done: Option<oneshot::Sender<Result<u64, String>>>,
}

/// A copy of one instance's volumes, run by [`Transfers::start_copy`].
struct Copy {
    task: tokio::task::JoinHandle<()>,
    result: Option<Result<u64, String>>,
}

#[derive(Default)]
pub struct Transfers {
    open: Mutex<HashMap<String, Open>>,
    /// Instance move id → its copy.
    copies: Mutex<HashMap<Id, Copy>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn close(transfer_id: &str, reason: &str) -> Out {
    Out::VolumeClose(pb::VolumeClose {
        transfer_id: transfer_id.to_owned(),
        error: reason.to_owned(),
        ..Default::default()
    })
}

struct Finish<'a>(&'a Transfers, &'a str);

impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.finish(self.1, Err("the copy was cancelled".into()));
    }
}

/// Where a copy stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyState {
    /// Not running in this process (never started, or lost to a restart).
    Missing,
    Running,
    Done(Result<u64, String>),
}

impl Transfers {
    /// Number of transfers under way (tests).
    pub fn len(&self) -> usize {
        crate::sync::lock(&self.open).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Start copying `volumes` (ids) from `source` to `dest` for move `key`,
    /// one after another, unless a copy for it is already running.
    pub fn start_copy(&self, state: &AppState, key: Id, source: Id, dest: Id, volumes: Vec<Id>) {
        let mut copies = crate::sync::lock(&self.copies);
        if copies.get(&key).is_some_and(|c| c.result.is_none()) {
            return;
        }
        let st = state.clone();
        let task = tokio::spawn(async move {
            let mut total = 0u64;
            let mut result = Ok(0);
            for v in volumes {
                let name = crate::deploy::desired::volume_name(v);
                match st.transfers.copy(&st, source, dest, &name).await {
                    Ok(n) => total += n,
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
            }
            let result = result.map(|_: u64| total);
            if let Some(c) = crate::sync::lock(&st.transfers.copies).get_mut(&key) {
                c.result = Some(result);
            }
        });
        copies.insert(key, Copy { task, result: None });
    }

    /// Where move `key`'s copy stands; a finished one is forgotten once read.
    pub fn copy_result(&self, key: Id) -> CopyState {
        let mut copies = crate::sync::lock(&self.copies);
        match copies.get(&key) {
            None => CopyState::Missing,
            Some(c) if c.result.is_none() => {
                if c.task.is_finished() {
                    copies.remove(&key);
                    CopyState::Missing
                } else {
                    CopyState::Running
                }
            }
            Some(_) => {
                let c = copies.remove(&key).expect("present");
                CopyState::Done(c.result.expect("finished"))
            }
        }
    }

    /// Stop move `key`'s copy (an abort).
    pub fn cancel_copy(&self, key: Id) {
        if let Some(c) = crate::sync::lock(&self.copies).remove(&key) {
            c.task.abort();
        }
    }

    /// Copy volume `volume` (its Docker name) from `source` to `dest` and
    /// return the archive's size.
    pub async fn copy(
        &self,
        state: &AppState,
        source: Id,
        dest: Id,
        volume: &str,
    ) -> Result<u64, String> {
        let id = Id::new().to_string();
        let (relay_tx, mut relay_rx) = mpsc::unbounded_channel::<(Id, Out)>();
        let (done_tx, mut done_rx) = oneshot::channel();
        let active = Arc::new(AtomicU64::new(now_ms()));
        crate::sync::lock(&self.open).insert(
            id.clone(),
            Open {
                source,
                dest,
                relay: relay_tx.clone(),
                in_flight: Arc::new(AtomicUsize::new(0)),
                active: active.clone(),
                done: Some(done_tx),
            },
        );
        // Messages to either node, in order; ends once the transfer is gone
        // (its sender dropped) and everything queued was sent.
        let relay = {
            let (state, id) = (state.clone(), id.clone());
            tokio::spawn(async move {
                while let Some((node, msg)) = relay_rx.recv().await {
                    if !state.hub.send(node, msg).await {
                        state
                            .transfers
                            .finish(&id, Err("a server went offline during the copy".into()));
                    }
                }
            })
        };
        let _ = relay_tx.send((
            dest,
            Out::VolumeImport(pb::VolumeImport {
                transfer_id: id.clone(),
                volume: volume.to_owned(),
            }),
        ));
        let _ = relay_tx.send((
            source,
            Out::VolumeExport(pb::VolumeExport {
                transfer_id: id.clone(),
                volume: volume.to_owned(),
            }),
        ));
        drop(relay_tx);
        // An aborted caller (cancel_copy) ends the transfer on both nodes.
        let _guard = Finish(self, &id);
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        let result = loop {
            tokio::select! {
                r = &mut done_rx => break r.unwrap_or_else(|_| Err("the copy was cancelled".into())),
                _ = tick.tick() => {
                    let idle = now_ms().saturating_sub(active.load(Ordering::Relaxed));
                    if idle > IDLE_TIMEOUT.as_millis() as u64 {
                        self.finish(&id, Err("the copy stalled".into()));
                    }
                }
            }
        };
        // Cancelled callers (an abort) leave the relay to drain on its own.
        let _ = relay.await;
        result
    }

    /// End a transfer: both nodes are told to drop it (a no-op for one that
    /// already finished it) and the caller gets `result`.
    fn finish(&self, id: &str, result: Result<u64, String>) {
        let Some(mut t) = crate::sync::lock(&self.open).remove(id) else {
            return;
        };
        if let Err(e) = &result {
            for n in [t.source, t.dest] {
                let _ = t.relay.send((n, close(id, e)));
            }
        }
        if let Some(done) = t.done.take() {
            let _ = done.send(result);
        }
    }

    /// A node's stream ended: its transfers fail.
    pub fn node_gone(&self, node: Id) {
        let ids: Vec<String> = crate::sync::lock(&self.open)
            .iter()
            .filter(|(_, t)| t.source == node || t.dest == node)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.finish(&id, Err("a server went offline during the copy".into()));
        }
    }

    /// Route a volume message from `node` without waiting. Returns a message
    /// for that node when the transfer is unknown to us.
    pub fn from_node(&self, node: Id, msg: In) -> Option<pb::ServerMessage> {
        let unknown = |transfer_id: &str| {
            Some(pb::ServerMessage {
                msg: Some(close(transfer_id, "unknown transfer")),
            })
        };
        match msg {
            In::VolumeData(m) => {
                let mut fail = None;
                {
                    let open = crate::sync::lock(&self.open);
                    let Some(t) = open.get(&m.transfer_id) else {
                        return (!m.data.is_empty())
                            .then(|| unknown(&m.transfer_id))
                            .flatten();
                    };
                    t.active.store(now_ms(), Ordering::Relaxed);
                    if node == t.source {
                        let len = m.data.len();
                        let over = len > CHUNK
                            || t.in_flight.fetch_add(len, Ordering::AcqRel) + len > WINDOW;
                        if over {
                            fail = Some("flow control violated");
                        } else {
                            let _ = t.relay.send((
                                t.dest,
                                Out::VolumeData(pb::VolumeData {
                                    transfer_id: m.transfer_id.clone(),
                                    data: m.data,
                                    credit: 0,
                                    eof: m.eof,
                                }),
                            ));
                        }
                    } else if node == t.dest && m.credit > 0 {
                        let credit = m.credit as usize;
                        let _ =
                            t.in_flight
                                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |f| {
                                    Some(f.saturating_sub(credit))
                                });
                        let _ = t.relay.send((
                            t.source,
                            Out::VolumeData(pb::VolumeData {
                                transfer_id: m.transfer_id.clone(),
                                credit: m.credit,
                                ..Default::default()
                            }),
                        ));
                    }
                }
                if let Some(why) = fail {
                    self.finish(&m.transfer_id, Err(why.into()));
                }
                None
            }
            In::VolumeClose(m) => {
                let (source, dest) = {
                    let open = crate::sync::lock(&self.open);
                    let t = open.get(&m.transfer_id)?;
                    (t.source, t.dest)
                };
                if node == dest {
                    let result = if m.ok {
                        Ok(m.bytes)
                    } else {
                        Err(format!("the new server: {}", m.error))
                    };
                    self.finish(&m.transfer_id, result);
                } else if node == source && !m.ok {
                    self.finish(&m.transfer_id, Err(format!("the old server: {}", m.error)));
                }
                None
            }
            _ => None,
        }
    }
}
