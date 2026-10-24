//! Shells and commands in a deployment's running container (`modula ssh`,
//! the dashboard's Shell tab).
//!
//! The client opens a WebSocket on `GET /api/deployments/{id}/exec`. The
//! command, terminal and size come in the query (`cmd` is a JSON array of
//! argv; none = a login shell). Frames:
//!
//! - client → server, binary: stdin bytes.
//! - client → server, text: `{"type":"resize","cols":…,"rows":…}` or
//!   `{"type":"eof"}` (stdin ends).
//! - server → client, binary: one channel byte ([`STDOUT`] or [`STDERR`])
//!   followed by output bytes. With a terminal everything is stdout.
//! - server → client, text: `{"type":"exit","code":…,"reason":…}` once,
//!   right before the close frame (`code` is null when the command did not
//!   exit by itself).
//!
//! The server relays over the node's gRPC stream (`ExecOpen` / `ExecData` /
//! `ExecResize` / `ExecClose`) to the node running the chosen replica, with
//! the tunnels' credit flow control (see [`crate::tunnel`]). Opening and
//! closing are audited (who, which container, the program run); what is typed
//! and printed never is.

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
use tokio::sync::mpsc;

use crate::audit::Entry;
use crate::auth::session::Actor;
use crate::authz::Grant;
use crate::error::{AppError, AppResult};
use crate::http::meta::RequestMeta;
use crate::state::AppState;
use crate::tunnel::{CHUNK, Credit, IDLE_TIMEOUT, WINDOW};

/// Output channel bytes (first byte of every binary frame to the client).
pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
const CREDIT_EVERY: usize = WINDOW / 4;
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// How often an open exec re-checks its deployment is still active.
const CHECK_EVERY: Duration = Duration::from_secs(30);
pub const MAX_PER_NODE: usize = 32;
pub const MAX_PER_USER: usize = 8;
const MAX_ARGS: usize = 64;
const MAX_CMD_BYTES: usize = 8 * 1024;

/// What the node sent about one exec, in order.
#[derive(Debug)]
enum Event {
    Opened(Result<(), String>),
    Data { stderr: bool, data: Bytes },
    Closed { code: Option<i32>, reason: String },
}

struct OpenExec {
    node: Id,
    user: Id,
    events: mpsc::UnboundedSender<Event>,
    /// Credit for our sends (stdin) to the node.
    credit: Arc<Credit>,
    /// Output from the node not yet written to the client (≤ WINDOW).
    buffered: Arc<AtomicUsize>,
}

/// Open execs, keyed by exec id.
pub struct Execs {
    open: Mutex<HashMap<String, OpenExec>>,
    /// [`CHECK_EVERY`], shortened by tests.
    check_every_ms: AtomicU64,
}

impl Default for Execs {
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

fn clip(s: &str) -> String {
    s.chars().take(300).collect()
}

impl Execs {
    fn reserve(&self, node: Id, user: Id) -> AppResult<Reserved> {
        let mut open = crate::sync::lock(&self.open);
        if open.values().filter(|e| e.node == node).count() >= MAX_PER_NODE {
            return Err(AppError::Conflict(
                "too many shells are open on this server; try again later".into(),
            ));
        }
        if open.values().filter(|e| e.user == user).count() >= MAX_PER_USER {
            return Err(AppError::Conflict(format!(
                "at most {MAX_PER_USER} shells can be open at once; close some first"
            )));
        }
        let id = Id::new().to_string();
        let (tx, rx) = mpsc::unbounded_channel();
        let credit = Arc::new(Credit::new());
        let buffered = Arc::new(AtomicUsize::new(0));
        open.insert(
            id.clone(),
            OpenExec {
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

    /// How often open execs re-check their deployment (tests shorten it).
    pub fn set_check_every(&self, every: Duration) {
        self.check_every_ms
            .store(every.as_millis() as u64, Ordering::Relaxed);
    }

    /// Number of open execs (tests, metrics).
    pub fn len(&self) -> usize {
        crate::sync::lock(&self.open).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The node's stream ended: every exec through it is gone.
    pub fn node_gone(&self, node: Id) {
        crate::sync::lock(&self.open).retain(|_, e| {
            if e.node == node {
                let _ = e.events.send(Event::Closed {
                    code: None,
                    reason: "the server went offline".into(),
                });
                false
            } else {
                true
            }
        });
    }

    /// Route an exec message from `node` without waiting. Returns a message
    /// to send back (closing an exec the node overran or no longer has).
    pub fn from_node(&self, node: Id, msg: In) -> Option<pb::ServerMessage> {
        let close = |exec_id: String, reason: &str| {
            Some(pb::ServerMessage {
                msg: Some(Out::ExecClose(pb::ExecClose {
                    exec_id,
                    reason: reason.into(),
                    ..Default::default()
                })),
            })
        };
        let mut open = crate::sync::lock(&self.open);
        match msg {
            In::ExecOpened(m) => {
                let e = open.get(&m.exec_id).filter(|e| e.node == node)?;
                let result = if m.ok { Ok(()) } else { Err(clip(&m.error)) };
                let _ = e.events.send(Event::Opened(result));
                None
            }
            In::ExecData(m) => {
                let Some(e) = open.get(&m.exec_id).filter(|e| e.node == node) else {
                    return (!m.data.is_empty())
                        .then(|| close(m.exec_id, "unknown exec"))
                        .flatten();
                };
                if m.credit > 0 {
                    e.credit.give(m.credit as usize);
                }
                if m.data.is_empty() {
                    return None;
                }
                let len = m.data.len();
                let over =
                    len > CHUNK || e.buffered.fetch_add(len, Ordering::AcqRel) + len > WINDOW;
                let event = Event::Data {
                    stderr: m.stderr,
                    data: Bytes::from(m.data),
                };
                if over || e.events.send(event).is_err() {
                    if let Some(e) = open.remove(&m.exec_id) {
                        let _ = e.events.send(Event::Closed {
                            code: None,
                            reason: "flow control violated".into(),
                        });
                    }
                    return close(m.exec_id, "flow control violated");
                }
                None
            }
            In::ExecClose(m) => {
                if open.get(&m.exec_id).is_some_and(|e| e.node == node)
                    && let Some(e) = open.remove(&m.exec_id)
                {
                    let reason = if m.reason.is_empty() {
                        "the command ended".to_owned()
                    } else {
                        clip(&m.reason)
                    };
                    let code = m.exited.then_some(m.exit_code);
                    let _ = e.events.send(Event::Closed { code, reason });
                }
                None
            }
            _ => None,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecQuery {
    /// Which replica's container (default: the first active one).
    pub replica: Option<i16>,
    /// The command as a JSON array of argv (default: a login shell).
    pub cmd: Option<String>,
    /// Allocate a terminal (default: true).
    pub tty: Option<bool>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
}

/// The command from the query's JSON array.
pub fn parse_cmd(raw: Option<&str>) -> AppResult<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    if raw.len() > MAX_CMD_BYTES {
        return Err(AppError::bad_request("the command is too long"));
    }
    let cmd: Vec<String> = serde_json::from_str(raw)
        .map_err(|_| AppError::bad_request("cmd must be a JSON array of strings"))?;
    if cmd.len() > MAX_ARGS {
        return Err(AppError::bad_request(format!(
            "at most {MAX_ARGS} arguments"
        )));
    }
    if cmd.first().is_some_and(|c| c.is_empty()) || cmd.iter().any(|a| a.contains('\0')) {
        return Err(AppError::bad_request("invalid command"));
    }
    Ok(cmd)
}

#[derive(sqlx::FromRow)]
struct Replica {
    ordinal: i16,
    node: Id,
}

/// Forgets an exec that never reached (or finished) its session and tells
/// the node, unless the node already closed it.
struct Guard {
    state: AppState,
    id: String,
    node: Id,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.state.execs.remove(&self.id) {
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
                    Out::ExecClose(pb::ExecClose {
                        exec_id: id,
                        reason: "closed by the client".into(),
                        ..Default::default()
                    }),
                )
                .await;
        });
    }
}

/// `GET /api/deployments/{deployment_id}/exec?replica=&cmd=&tty=&cols=&rows=`
/// — upgrade to a WebSocket attached to a command in the deployment's
/// running container.
#[allow(clippy::too_many_arguments)]
pub async fn open(
    State(state): State<AppState>,
    actor: Actor,
    grant: Grant,
    meta: RequestMeta,
    headers: HeaderMap,
    Query(q): Query<ExecQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<Response> {
    // As for tunnels: a cookie session only from our own pages.
    if actor.api_token.is_none() {
        let same_origin = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| state.is_dashboard_origin(o));
        if !same_origin {
            return Err(AppError::Forbidden);
        }
    }
    let deployment = grant.id();
    let workspace = grant.workspace_id()?;
    let cmd = parse_cmd(q.cmd.as_deref())?;
    let tty = q.tty.unwrap_or(true);
    let (instance, status): (Id, String) =
        sqlx::query_as("SELECT service_instance_id, status FROM deployments WHERE id = $1")
            .bind(deployment)
            .fetch_one(&state.db)
            .await?;
    if status != "active" {
        return Err(AppError::Conflict("this deployment is not running".into()));
    }
    let replicas: Vec<Replica> = sqlx::query_as(
        "SELECT ordinal, node_id AS node FROM deployment_replicas
         WHERE deployment_id = $1 AND status = 'active' AND node_id IS NOT NULL
         ORDER BY ordinal",
    )
    .bind(deployment)
    .fetch_all(&state.db)
    .await?;
    let replica = match q.replica {
        Some(n) => replicas.iter().find(|r| r.ordinal == n).ok_or_else(|| {
            AppError::Conflict(format!("replica {n} of this deployment is not running"))
        })?,
        None => replicas
            .iter()
            .find(|r| state.hub.is_connected(r.node))
            .or(replicas.first())
            .ok_or_else(|| AppError::Conflict("this deployment is not running".into()))?,
    };
    let (ordinal, node) = (replica.ordinal, replica.node);
    if !state.hub.is_connected(node) {
        return Err(AppError::Conflict(
            "the server running this replica is offline".into(),
        ));
    }

    let Reserved {
        id,
        mut events,
        credit,
        buffered,
    } = state.execs.reserve(node, actor.user_id)?;
    let guard = Guard {
        state: state.clone(),
        id: id.clone(),
        node,
    };
    let sent = state
        .hub
        .send(
            node,
            Out::ExecOpen(pb::ExecOpen {
                exec_id: id.clone(),
                deployment_id: deployment.to_string(),
                cmd: cmd.clone(),
                tty,
                cols: q.cols.map_or(0, u32::from),
                rows: q.rows.map_or(0, u32::from),
            }),
        )
        .await;
    if !sent {
        return Err(AppError::Conflict(
            "the server running this replica is offline".into(),
        ));
    }
    match tokio::time::timeout(OPEN_TIMEOUT, events.recv()).await {
        Ok(Some(Event::Opened(Ok(())))) => {}
        Ok(Some(Event::Opened(Err(e)))) => {
            return Err(AppError::Conflict(format!("cannot start the command: {e}")));
        }
        Ok(Some(Event::Closed { reason, .. })) => {
            return Err(AppError::Conflict(format!(
                "cannot start the command: {reason}"
            )));
        }
        Ok(_) | Err(_) => {
            return Err(AppError::Unavailable(
                "the server running this replica did not answer".into(),
            ));
        }
    }

    // The program only: arguments can carry credentials (`psql postgres://…`).
    let command = cmd.first().map(|p| p.chars().take(200).collect::<String>());
    Entry::new("exec.open", &meta)
        .actor(&actor)
        .workspace(workspace)
        .target("deployment", deployment)
        .metadata(json!({
            "exec": id,
            "serviceInstance": instance,
            "replica": ordinal,
            "node": node,
            "command": command,
            "tty": tty,
        }))
        .record(&state.db)
        .await?;
    tracing::info!(exec = %id, %deployment, %node, replica = ordinal, "exec opened");

    let session = Session {
        state,
        actor,
        meta,
        workspace,
        deployment,
        guard,
        credit,
        buffered,
        node,
    };
    Ok(ws
        .max_message_size(4 * CHUNK)
        .on_upgrade(move |socket| session.run(socket, events)))
}

/// A control frame from the client.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Control {
    Resize { cols: u16, rows: u16 },
    Eof,
}

struct Session {
    state: AppState,
    actor: Actor,
    meta: RequestMeta,
    workspace: Id,
    deployment: Id,
    node: Id,
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
        let (in_bytes, out_bytes) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let (mut sink, mut stream) = socket.split();
        let (state, node, id) = (self.state.clone(), self.node, self.guard.id.clone());

        // Client → node.
        let mut up = {
            let (state, id, credit, last, bytes) = (
                state.clone(),
                id.clone(),
                self.credit.clone(),
                last_active.clone(),
                in_bytes.clone(),
            );
            tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    let data = match msg {
                        Message::Binary(b) => b,
                        Message::Text(t) => {
                            let out = match serde_json::from_str::<Control>(&t) {
                                Ok(Control::Resize { cols, rows }) => {
                                    Out::ExecResize(pb::ExecResize {
                                        exec_id: id.clone(),
                                        cols: u32::from(cols),
                                        rows: u32::from(rows),
                                    })
                                }
                                Ok(Control::Eof) => Out::ExecData(pb::ExecData {
                                    exec_id: id.clone(),
                                    eof: true,
                                    ..Default::default()
                                }),
                                Err(_) => return "unknown control frame".to_owned(),
                            };
                            last.store(now_secs(started), Ordering::Relaxed);
                            if !state.hub.send(node, out).await {
                                return "the server went offline".to_owned();
                            }
                            continue;
                        }
                        Message::Close(_) => break,
                        Message::Ping(_) | Message::Pong(_) => continue,
                    };
                    for chunk in data.chunks(CHUNK) {
                        credit.take(chunk.len()).await;
                        last.store(now_secs(started), Ordering::Relaxed);
                        bytes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        let msg = Out::ExecData(pb::ExecData {
                            exec_id: id.clone(),
                            data: chunk.to_vec(),
                            ..Default::default()
                        });
                        if !state.hub.send(node, msg).await {
                            return "the server went offline".to_owned();
                        }
                    }
                }
                "closed by the client".to_owned()
            })
        };

        // Node → client; ends when the exec closes.
        let mut down = {
            let (state, id, buffered, last, bytes) = (
                state.clone(),
                id.clone(),
                self.buffered.clone(),
                last_active.clone(),
                out_bytes.clone(),
            );
            tokio::spawn(async move {
                let mut owed = 0usize;
                let (code, reason) = loop {
                    match events.recv().await {
                        Some(Event::Data { stderr, data }) => {
                            let n = data.len();
                            last.store(now_secs(started), Ordering::Relaxed);
                            bytes.fetch_add(n as u64, Ordering::Relaxed);
                            let mut frame = Vec::with_capacity(n + 1);
                            frame.push(if stderr { STDERR } else { STDOUT });
                            frame.extend_from_slice(&data);
                            if sink.send(Message::Binary(frame.into())).await.is_err() {
                                break (None, "closed by the client".to_owned());
                            }
                            buffered.fetch_sub(n, Ordering::AcqRel);
                            owed += n;
                            if owed >= CREDIT_EVERY {
                                let msg = Out::ExecData(pb::ExecData {
                                    exec_id: id.clone(),
                                    credit: std::mem::take(&mut owed) as u32,
                                    ..Default::default()
                                });
                                state.hub.send(node, msg).await;
                            }
                        }
                        Some(Event::Opened(_)) => {}
                        Some(Event::Closed { code, reason }) => break (code, reason),
                        None => break (None, "closed".to_owned()),
                    }
                };
                let exit = json!({ "type": "exit", "code": code, "reason": reason });
                let _ = sink.send(Message::Text(exit.to_string().into())).await;
                let _ = sink
                    .send(Message::Close(Some(CloseFrame {
                        code: 1000,
                        reason: reason.chars().take(120).collect::<String>().into(),
                    })))
                    .await;
                (code, reason)
            })
        };

        let every = state.execs.check_every_ms.load(Ordering::Relaxed);
        let mut check = tokio::time::interval(Duration::from_millis(every.max(10)));
        check.tick().await;
        let (code, reason) = loop {
            tokio::select! {
                r = &mut up => break (None, r.unwrap_or_else(|_| "closed".into())),
                r = &mut down => break r.unwrap_or_else(|_| (None, "closed".into())),
                _ = state.shutdown.cancelled() => break (None, "the control plane is restarting".into()),
                _ = check.tick() => {
                    let idle = now_secs(started).saturating_sub(last_active.load(Ordering::Relaxed));
                    if idle >= IDLE_TIMEOUT.as_secs() {
                        break (None, "idle for 30 minutes".into());
                    }
                    let active: bool = sqlx::query_scalar(
                        "SELECT EXISTS (SELECT 1 FROM deployments WHERE id = $1 AND status = 'active')",
                    )
                    .bind(self.deployment)
                    .fetch_one(&state.db)
                    .await
                    .unwrap_or(true);
                    if !active {
                        break (None, "the deployment was removed".into());
                    }
                }
            }
        };
        up.abort();
        if !down.is_finished() {
            // Give the client its exit message and close frame.
            if let Some(e) = crate::sync::lock(&state.execs.open).get(&id) {
                let _ = e.events.send(Event::Closed {
                    code: None,
                    reason: reason.clone(),
                });
            }
            let _ = tokio::time::timeout(Duration::from_secs(2), &mut down).await;
            down.abort();
        }
        // Forgets the exec and tells the node (unless it closed it).
        drop(self.guard);

        let (bytes_in, bytes_out) = (
            in_bytes.load(Ordering::Relaxed),
            out_bytes.load(Ordering::Relaxed),
        );
        tracing::info!(exec = %id, %reason, bytes_in, bytes_out, "exec closed");
        let _ = Entry::new("exec.close", &self.meta)
            .actor(&self.actor)
            .workspace(self.workspace)
            .target("deployment", self.deployment)
            .metadata(json!({
                "exec": id,
                "exitCode": code,
                "reason": reason,
                "bytesIn": bytes_in,
                "bytesOut": bytes_out,
                "durationSecs": started.elapsed().as_secs(),
            }))
            .record(&state.db)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_json_argv() {
        assert!(parse_cmd(None).unwrap().is_empty());
        assert_eq!(
            parse_cmd(Some(r#"["ls","-la","/app dir"]"#)).unwrap(),
            ["ls", "-la", "/app dir"]
        );
        assert!(parse_cmd(Some("ls -la")).is_err());
        assert!(parse_cmd(Some(r#"[""]"#)).is_err());
        assert!(parse_cmd(Some(r#"["a\u0000"]"#)).is_err());
        let many = serde_json::to_string(&vec!["x"; MAX_ARGS + 1]).unwrap();
        assert!(parse_cmd(Some(&many)).is_err());
    }

    #[test]
    fn limits_per_user_and_node() {
        let e = Execs::default();
        let (node, user) = (Id::new(), Id::new());
        let held: Vec<_> = (0..MAX_PER_USER)
            .map(|_| e.reserve(node, user).unwrap())
            .collect();
        assert!(e.reserve(node, user).is_err());
        // Another user still fits on the node until it is full.
        for _ in MAX_PER_USER..MAX_PER_NODE {
            let _ = e.reserve(node, Id::new()).unwrap();
        }
        assert!(e.reserve(node, Id::new()).is_err());
        assert!(e.reserve(Id::new(), Id::new()).is_ok());
        e.node_gone(node);
        assert_eq!(e.len(), 1);
        drop(held);
    }

    #[test]
    fn overrunning_the_window_closes_the_exec() {
        let e = Execs::default();
        let node = Id::new();
        let r = e.reserve(node, Id::new()).unwrap();
        let data = |n: usize| {
            In::ExecData(pb::ExecData {
                exec_id: r.id.clone(),
                data: vec![0; n],
                ..Default::default()
            })
        };
        for _ in 0..WINDOW / CHUNK {
            assert!(e.from_node(node, data(CHUNK)).is_none());
        }
        // Another node can't touch it.
        assert!(e.from_node(Id::new(), data(1)).is_some());
        assert!(e.from_node(node, data(1)).is_some());
        assert!(e.is_empty());
    }
}
