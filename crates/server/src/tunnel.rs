//! Private TCP tunnels to a service's running container (`modula
//! connect`), so databases are reachable from a laptop without a public port.
//!
//! The client opens a WebSocket on `GET /api/service-instances/{id}/tunnel`;
//! binary frames are raw TCP bytes. The server relays them over the node's
//! existing gRPC stream (`TunnelOpen` / `TunnelData` / `TunnelClose`) to the
//! node running the instance's active deployment, which dials that container on
//! its environment network.
//!
//! Flow control: credits, per tunnel and per direction. A sender may have at
//! most [`WINDOW`] bytes in flight and waits for credit beyond that; the
//! receiver returns credit (`TunnelData.credit`) once it has handed bytes on
//! (written them to the WebSocket here, to the container's socket on the node).
//! Because the peer can never have more than a window outstanding, the node
//! stream's reader can queue incoming data without waiting and without
//! dropping, so one slow client never stalls the shared stream; a peer that
//! exceeds its window has the tunnel closed.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use modula_id::Id;
use modula_proto::node::v1 as pb;
use modula_proto::node::v1::node_message::Msg as In;
use modula_proto::node::v1::server_message::Msg as Out;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{Semaphore, mpsc};

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;

/// Largest data chunk in one `TunnelData`.
pub const CHUNK: usize = 64 * 1024;
/// Bytes a sender may have in flight per tunnel and direction.
pub const WINDOW: usize = 256 * 1024;
/// Return credit once this much has been consumed (a quarter window).
const CREDIT_EVERY: usize = WINDOW / 4;
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// No bytes either way for this long closes the tunnel.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How often an open tunnel re-checks its deployment is still the active one.
const CHECK_EVERY: Duration = Duration::from_secs(30);
const MAX_PER_NODE: usize = 64;
const MAX_PER_USER: usize = 16;

/// What the node sent about one tunnel, in order.
#[derive(Debug)]
enum Event {
    Opened(Result<(), String>),
    Data(Bytes),
    Closed(String),
}

/// A byte window: `take` waits for room, `give` returns credit (never more
/// than is outstanding, so a confused peer can't grow the window).
pub struct Credit {
    room: Semaphore,
    in_flight: AtomicUsize,
}

impl Credit {
    pub fn new() -> Self {
        Self {
            room: Semaphore::new(WINDOW),
            in_flight: AtomicUsize::new(0),
        }
    }

    /// Wait until `n` (≤ [`CHUNK`]) more bytes may be sent.
    pub async fn take(&self, n: usize) {
        if let Ok(p) = self.room.acquire_many(n as u32).await {
            p.forget();
            self.in_flight.fetch_add(n, Ordering::AcqRel);
        }
    }

    pub fn give(&self, n: usize) {
        let given = self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |f| Some(f - n.min(f)))
            .map_or(0, |f| n.min(f));
        self.room.add_permits(given);
    }
}

impl Default for Credit {
    fn default() -> Self {
        Self::new()
    }
}

struct OpenTunnel {
    node: Id,
    user: Id,
    events: mpsc::UnboundedSender<Event>,
    /// Credit for our sends to the node.
    credit: Arc<Credit>,
    /// Bytes from the node not yet written to the client (≤ WINDOW).
    buffered: Arc<AtomicUsize>,
}

/// Open tunnels, keyed by tunnel id.
pub struct Tunnels {
    open: Mutex<HashMap<String, OpenTunnel>>,
    /// [`CHECK_EVERY`], shortened by tests.
    check_every_ms: AtomicU64,
}

impl Default for Tunnels {
    fn default() -> Self {
        Self {
            open: Mutex::default(),
            check_every_ms: AtomicU64::new(CHECK_EVERY.as_millis() as u64),
        }
    }
}

struct Reserved {
    id: String,
    events: mpsc::UnboundedReceiver<Event>,
    credit: Arc<Credit>,
    buffered: Arc<AtomicUsize>,
}

impl Tunnels {
    fn reserve(&self, node: Id, user: Id) -> AppResult<Reserved> {
        let mut open = crate::sync::lock(&self.open);
        if open.values().filter(|t| t.node == node).count() >= MAX_PER_NODE {
            return Err(AppError::Conflict(
                "too many tunnels are open to this server; try again later".into(),
            ));
        }
        if open.values().filter(|t| t.user == user).count() >= MAX_PER_USER {
            return Err(AppError::Conflict(format!(
                "at most {MAX_PER_USER} tunnels can be open at once; close some first"
            )));
        }
        let id = Id::new().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        let credit = Arc::new(Credit::new());
        let buffered = Arc::new(AtomicUsize::new(0));
        open.insert(
            id.clone(),
            OpenTunnel {
                node,
                user,
                events: tx,
                credit: credit.clone(),
                buffered: buffered.clone(),
            },
        );
        Ok(Reserved {
            id,
            events: rx,
            credit,
            buffered,
        })
    }

    fn remove(&self, id: &str) -> bool {
        crate::sync::lock(&self.open).remove(id).is_some()
    }

    /// How often open tunnels re-check their deployment (tests shorten it).
    pub fn set_check_every(&self, every: Duration) {
        self.check_every_ms
            .store(every.as_millis() as u64, Ordering::Relaxed);
    }

    /// Number of open tunnels (tests, metrics).
    pub fn len(&self) -> usize {
        crate::sync::lock(&self.open).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The node's stream ended: every tunnel through it is gone.
    pub fn node_gone(&self, node: Id) {
        let mut open = crate::sync::lock(&self.open);
        open.retain(|_, t| {
            if t.node == node {
                let _ = t
                    .events
                    .send(Event::Closed("the server went offline".into()));
                false
            } else {
                true
            }
        });
    }

    /// Route a tunnel message from `node` without waiting. Returns a message
    /// to send back (closing a tunnel the node overran or no longer has).
    pub fn from_node(&self, node: Id, msg: In) -> Option<pb::ServerMessage> {
        let close = |tunnel_id: String, reason: &str| {
            Some(pb::ServerMessage {
                msg: Some(Out::TunnelClose(pb::TunnelClose {
                    tunnel_id,
                    reason: reason.into(),
                })),
            })
        };
        let mut open = crate::sync::lock(&self.open);
        match msg {
            In::TunnelOpened(m) => {
                let t = open.get(&m.tunnel_id).filter(|t| t.node == node)?;
                let result = if m.ok {
                    Ok(())
                } else {
                    Err(m.error.chars().take(300).collect())
                };
                let _ = t.events.send(Event::Opened(result));
                None
            }
            In::TunnelData(m) => {
                let Some(t) = open.get(&m.tunnel_id).filter(|t| t.node == node) else {
                    // Late data for a tunnel we already closed: tell the node
                    // again only if it is still sending real bytes.
                    return (!m.data.is_empty())
                        .then(|| close(m.tunnel_id, "unknown tunnel"))
                        .flatten();
                };
                if m.credit > 0 {
                    t.credit.give(m.credit as usize);
                }
                if m.data.is_empty() {
                    return None;
                }
                let len = m.data.len();
                let over =
                    len > CHUNK || t.buffered.fetch_add(len, Ordering::AcqRel) + len > WINDOW;
                if over || t.events.send(Event::Data(Bytes::from(m.data))).is_err() {
                    if let Some(t) = open.remove(&m.tunnel_id) {
                        let _ = t.events.send(Event::Closed("flow control violated".into()));
                    }
                    return close(m.tunnel_id, "flow control violated");
                }
                None
            }
            In::TunnelClose(m) => {
                if open.get(&m.tunnel_id).is_some_and(|t| t.node == node)
                    && let Some(t) = open.remove(&m.tunnel_id)
                {
                    let reason = if m.reason.is_empty() {
                        "closed by the server".to_owned()
                    } else {
                        m.reason.chars().take(300).collect()
                    };
                    let _ = t.events.send(Event::Closed(reason));
                }
                None
            }
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TunnelQuery {
    /// Target port in the container; defaults to the service's port.
    pub port: Option<u16>,
}

#[derive(sqlx::FromRow)]
struct Target {
    deployment: Option<Id>,
    node: Option<Id>,
    port: Option<i32>,
    workspace: Id,
}

/// Cleans up a tunnel that never reached (or finished) its pump: forgets it
/// and tells the node, unless the node already closed it.
struct Guard {
    state: AppState,
    id: String,
    node: Id,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.state.tunnels.remove(&self.id) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (state, id, node) = (self.state.clone(), std::mem::take(&mut self.id), self.node);
        rt.spawn(async move {
            state
                .hub
                .send(
                    node,
                    Out::TunnelClose(pb::TunnelClose {
                        tunnel_id: id,
                        reason: "closed by the client".into(),
                    }),
                )
                .await;
        });
    }
}

/// `GET /api/service-instances/{instance_id}/tunnel?port=` — upgrade to a
/// WebSocket carrying one TCP connection to the active deployment's container.
#[allow(clippy::too_many_arguments)]
pub async fn open(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    headers: HeaderMap,
    Query(q): Query<TunnelQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    // Browsers don't apply the same-origin policy to WebSockets, so a cookie
    // session is only accepted from our own pages. API tokens can't be ridden
    // cross-site and need no Origin (the CLI sends none).
    if actor.api_token.is_none() {
        let same_origin = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| state.is_dashboard_origin(o));
        if !same_origin {
            return Err(AppError::Forbidden);
        }
    }
    let instance = grant.id();
    let t: Target = sqlx::query_as(
        "SELECT d.id AS deployment, d.node_id AS node, (i.config->>'port')::int AS port,
                p.workspace_id AS workspace
         FROM service_instances i
         JOIN services s ON s.id = i.service_id
         JOIN projects p ON p.id = s.project_id
         LEFT JOIN deployments d ON d.service_instance_id = i.id AND d.status = 'active'
         WHERE i.id = $1",
    )
    .bind(instance)
    .fetch_one(&state.db)
    .await?;
    let (Some(deployment), Some(node)) = (t.deployment, t.node) else {
        return Err(AppError::Conflict(
            "this service has no running deployment".into(),
        ));
    };
    let port = match q.port.or(t.port.and_then(|p| u16::try_from(p).ok())) {
        Some(p) if p > 0 => p,
        _ => return Err(AppError::bad_request("port is required")),
    };
    let capable: Option<bool> = sqlx::query_scalar(
        "SELECT coalesce(info->'capabilities' ? 'tunnel', false) FROM nodes WHERE id = $1",
    )
    .bind(node)
    .fetch_optional(&state.db)
    .await?;
    if capable != Some(true) {
        return Err(AppError::Conflict(
            "the server running this service doesn't support tunnels yet; update modula-node"
                .into(),
        ));
    }
    if !state.hub.is_connected(node) {
        return Err(AppError::Conflict(
            "the server running this service is offline".into(),
        ));
    }

    let Reserved {
        id,
        mut events,
        credit,
        buffered,
    } = state.tunnels.reserve(node, actor.user_id)?;
    let guard = Guard {
        state: state.clone(),
        id: id.clone(),
        node,
    };
    let sent = state
        .hub
        .send(
            node,
            Out::TunnelOpen(pb::TunnelOpen {
                tunnel_id: id.clone(),
                deployment_id: deployment.to_string(),
                port: u32::from(port),
            }),
        )
        .await;
    if !sent {
        return Err(AppError::Conflict(
            "the server running this service is offline".into(),
        ));
    }
    match tokio::time::timeout(OPEN_TIMEOUT, events.recv()).await {
        Ok(Some(Event::Opened(Ok(())))) => {}
        Ok(Some(Event::Opened(Err(e)))) => {
            return Err(AppError::Conflict(format!("cannot connect: {e}")));
        }
        Ok(Some(Event::Closed(reason))) => {
            return Err(AppError::Conflict(format!("cannot connect: {reason}")));
        }
        Ok(_) | Err(_) => {
            return Err(AppError::Unavailable(
                "the server running this service did not answer".into(),
            ));
        }
    }

    Entry::new("tunnel.open", &meta)
        .actor(&actor)
        .workspace(t.workspace)
        .target("service_instance", instance)
        .metadata(json!({ "tunnel": id, "deployment": deployment, "node": node, "port": port }))
        .record(&state.db)
        .await?;
    tracing::info!(tunnel = %id, %instance, %node, port, "tunnel opened");

    let session = Session {
        state,
        actor,
        meta,
        workspace: t.workspace,
        instance,
        deployment,
        node,
        port,
        guard,
        credit,
        buffered,
    };
    Ok(ws
        .max_message_size(4 * CHUNK)
        .on_upgrade(move |socket| session.run(socket, events)))
}

struct Session {
    state: AppState,
    actor: Actor,
    meta: RequestMeta,
    workspace: Id,
    instance: Id,
    deployment: Id,
    node: Id,
    port: u16,
    guard: Guard,
    credit: Arc<Credit>,
    buffered: Arc<AtomicUsize>,
}

fn now_secs(start: Instant) -> u64 {
    start.elapsed().as_secs()
}

impl Session {
    async fn run(self, socket: WebSocket, mut events: mpsc::UnboundedReceiver<Event>) {
        let started = Instant::now();
        let last_active = Arc::new(AtomicU64::new(0));
        let (up_bytes, down_bytes) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (mut sink, mut stream) = socket.split();
        let (state, node, id) = (self.state.clone(), self.node, self.guard.id.clone());

        // Client → node.
        let mut up = {
            let (state, id, credit, last, bytes) = (
                state.clone(),
                id.clone(),
                self.credit.clone(),
                last_active.clone(),
                up_bytes.clone(),
            );
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let data = match msg {
                        Message::Binary(b) => b,
                        Message::Close(_) => break,
                        Message::Text(_) => return "text frames are not supported".to_owned(),
                        Message::Ping(_) | Message::Pong(_) => continue,
                    };
                    for chunk in data.chunks(CHUNK) {
                        credit.take(chunk.len()).await;
                        last.store(now_secs(started), Ordering::Relaxed);
                        bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        let msg = Out::TunnelData(pb::TunnelData {
                            tunnel_id: id.clone(),
                            data: chunk.to_vec(),
                            credit: 0,
                        });
                        if !state.hub.send(node, msg).await {
                            return "the server went offline".to_owned();
                        }
                    }
                }
                "closed by the client".to_owned()
            })
        };

        // Node → client; ends when the node closes the tunnel.
        let mut down = {
            let (state, id, buffered, last, bytes) = (
                state.clone(),
                id.clone(),
                self.buffered.clone(),
                last_active.clone(),
                down_bytes.clone(),
            );
            tokio::spawn(async move {
                let mut owed = 0usize;
                let reason = loop {
                    match events.recv().await {
                        Some(Event::Data(b)) => {
                            let n = b.len();
                            last.store(now_secs(started), Ordering::Relaxed);
                            bytes.fetch_add(n as u64, Ordering::Relaxed);
                            if sink.send(Message::Binary(b)).await.is_err() {
                                break "closed by the client".to_owned();
                            }
                            buffered.fetch_sub(n, Ordering::AcqRel);
                            owed += n;
                            if owed >= CREDIT_EVERY {
                                let msg = Out::TunnelData(pb::TunnelData {
                                    tunnel_id: id.clone(),
                                    data: Vec::new(),
                                    credit: std::mem::take(&mut owed) as u32,
                                });
                                state.hub.send(node, msg).await;
                            }
                        }
                        Some(Event::Opened(_)) => {}
                        Some(Event::Closed(reason)) => break reason,
                        None => break "closed".to_owned(),
                    }
                };
                let _ = sink
                    .send(Message::Close(Some(CloseFrame {
                        code: 1000,
                        reason: reason.chars().take(120).collect::<String>().into(),
                    })))
                    .await;
                reason
            })
        };

        let every = state.tunnels.check_every_ms.load(Ordering::Relaxed);
        let mut check = tokio::time::interval(Duration::from_millis(every.max(10)));
        check.tick().await;
        let reason = loop {
            tokio::select! {
                r = &mut up => break r.unwrap_or_else(|_| "closed".into()),
                r = &mut down => break r.unwrap_or_else(|_| "closed".into()),
                _ = state.shutdown.cancelled() => break "the control plane is restarting".into(),
                _ = check.tick() => {
                    let idle = now_secs(started).saturating_sub(last_active.load(Ordering::Relaxed));
                    if idle >= IDLE_TIMEOUT.as_secs() {
                        break "idle for 30 minutes".into();
                    }
                    let active: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM deployments WHERE id = $1 AND status = 'active')",
                    )
                    .bind(self.deployment)
                    .fetch_one(&state.db)
                    .await
                    .unwrap_or(true);
                    if !active {
                        break "the deployment was removed".into();
                    }
                }
            }
        };
        up.abort();
        if !down.is_finished() {
            // Give the client its close frame with the reason.
            if let Some(t) = crate::sync::lock(&state.tunnels.open).get(&id) {
                let _ = t.events.send(Event::Closed(reason.clone()));
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut down).await;
            down.abort();
        }
        // Forgets the tunnel and tells the node (unless it closed it).
        drop(self.guard);

        let (up, down) = (
            up_bytes.load(Ordering::Relaxed),
            down_bytes.load(Ordering::Relaxed),
        );
        tracing::info!(tunnel = %id, %reason, up, down, "tunnel closed");
        let _ = Entry::new("tunnel.close", &self.meta)
            .actor(&self.actor)
            .workspace(self.workspace)
            .target("service_instance", self.instance)
            .metadata(json!({
                "tunnel": id,
                "port": self.port,
                "reason": reason,
                "bytesUp": up,
                "bytesDown": down,
                "durationSecs": started.elapsed().as_secs(),
            }))
            .record(&state.db)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn credit_never_exceeds_the_window() {
        let c = Credit::new();
        c.take(WINDOW).await;
        // Over-crediting is capped at what is outstanding.
        c.give(WINDOW * 4);
        assert_eq!(c.room.available_permits(), WINDOW);
        c.take(10).await;
        c.give(4);
        assert_eq!(c.room.available_permits(), WINDOW - 6);
    }

    #[test]
    fn overrunning_the_window_closes_the_tunnel() {
        let t = Tunnels::default();
        let node = Id::new();
        let r = t.reserve(node, Id::new()).unwrap();
        let data = |n: usize| {
            In::TunnelData(pb::TunnelData {
                tunnel_id: r.id.clone(),
                data: vec![0; n],
                credit: 0,
            })
        };
        for _ in 0..WINDOW / CHUNK {
            assert!(t.from_node(node, data(CHUNK)).is_none());
        }
        // Another node can't touch it.
        assert!(t.from_node(Id::new(), data(1)).is_some());
        assert!(t.from_node(node, data(1)).is_some());
        assert!(t.is_empty());
    }
}
