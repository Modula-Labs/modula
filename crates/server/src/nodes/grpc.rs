//! The `modula.node.v1.Node` gRPC service that `modula-node` dials.

use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use futures::{Stream, StreamExt};
use modula_crypto::token::{self, prefix};
use modula_id::Id;
use modula_proto::node::v1 as pb;
use modula_proto::node::v1::node_message::Msg as In;
use modula_proto::node::v1::server_message::Msg as Out;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};

use crate::audit::Entry;
use crate::deploy;
use crate::http::meta::RequestMeta;
use crate::ratelimit::Limit;
use crate::state::AppState;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const RESYNC_EVERY: Duration = Duration::from_secs(60);

pub struct NodeService {
    pub state: AppState,
}

fn internal(e: impl std::fmt::Display) -> Status {
    tracing::error!(error = %e, "node rpc failed");
    Status::internal("internal error")
}

/// Hostnames end up in the UI; keep them to a safe DNS-ish subset.
fn clean_hostname(raw: &str) -> String {
    let h: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .take(63)
        .collect();
    if h.is_empty() { "server".into() } else { h }
}

fn peer_ip(addr: Option<SocketAddr>) -> std::net::IpAddr {
    addr.map_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), |a| {
        a.ip()
    })
}

type AttachStream = Pin<Box<dyn Stream<Item = Result<pb::ServerMessage, Status>> + Send>>;

#[tonic::async_trait]
impl pb::node_server::Node for NodeService {
    async fn get_ca_bundle(
        &self,
        _: Request<pb::GetCaBundleRequest>,
    ) -> Result<Response<pb::CaBundle>, Status> {
        let pki = self.state.pki().await.map_err(internal)?;
        Ok(Response::new(pb::CaBundle {
            root_der: pki.root_der().to_vec(),
            intermediate_der: pki.intermediate_der().to_vec(),
        }))
    }

    async fn enroll(
        &self,
        req: Request<pb::EnrollRequest>,
    ) -> Result<Response<pb::EnrollResponse>, Status> {
        if !self.state.private.node_peer_allowed(req.remote_addr()) {
            return Err(Status::permission_denied(
                "this instance only accepts servers through its Cloudflare Tunnel",
            ));
        }
        let ip = peer_ip(req.remote_addr());
        if self
            .state
            .limiter
            .check("node_enroll", &ip.to_string(), Limit::per_minute(10))
            .await
            .is_err()
        {
            return Err(Status::resource_exhausted("too many enrollment attempts"));
        }
        let req = req.into_inner();
        if !token::is_well_formed(&req.join_token, prefix::NODE_JOIN) {
            return Err(Status::permission_denied("invalid or expired join token"));
        }
        let pki = self.state.pki().await.map_err(internal)?;
        let node_id = Id::new();
        // Validate the CSR before burning the token.
        let issued = pki
            .issue_node_cert(node_id, &req.csr_der)
            .map_err(|_| Status::invalid_argument("invalid certificate signing request"))?;
        let name = clean_hostname(&req.hostname);
        let version: String = req.node_version.chars().take(64).collect();

        let mut tx = self.state.db.begin().await.map_err(internal)?;
        // A workspace's token (Hetzner provisioning) enrolls a server only
        // that workspace places on; NULL = shared.
        let token: Option<(Id, Option<Id>)> = sqlx::query_as(
            "UPDATE node_join_tokens SET used_at = now()
             WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
             RETURNING id, workspace_id",
        )
        .bind(token::hash(&req.join_token).as_bytes())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
        let Some((token_id, workspace)) = token else {
            return Err(Status::permission_denied("invalid or expired join token"));
        };
        sqlx::query(
            "INSERT INTO nodes (id, workspace_id, name, cert_serial, cert_expires_at, node_version)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(node_id)
        .bind(workspace)
        .bind(&name)
        .bind(&issued.serial_hex)
        .bind(issued.not_after)
        .bind(&version)
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        sqlx::query("UPDATE node_join_tokens SET node_id = $2 WHERE id = $1")
            .bind(token_id)
            .bind(node_id)
            .execute(&mut *tx)
            .await
            .map_err(internal)?;
        let meta = RequestMeta {
            ip,
            user_agent: Some(format!("modula-node/{version}")),
        };
        Entry::new("node.enroll", &meta)
            .target("node", node_id)
            .metadata(serde_json::json!({ "name": name, "joinToken": token_id }))
            .record(&mut *tx)
            .await
            .map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        tracing::info!(%node_id, %name, "node enrolled");
        Ok(Response::new(pb::EnrollResponse {
            node_id: node_id.to_string(),
            cert_der: issued.der,
            intermediate_der: pki.intermediate_der().to_vec(),
            root_der: pki.root_der().to_vec(),
        }))
    }

    type AttachStream = AttachStream;

    async fn attach(
        &self,
        req: Request<Streaming<pb::NodeMessage>>,
    ) -> Result<Response<AttachStream>, Status> {
        let state = self.state.clone();
        if !state.private.node_peer_allowed(req.remote_addr()) {
            return Err(Status::permission_denied(
                "this instance only accepts servers through its Cloudflare Tunnel",
            ));
        }
        let certs = req
            .peer_certs()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let leaf = certs
            .first()
            .ok_or_else(|| Status::unauthenticated("client certificate required"))?;
        let (node, serial) = crate::pki::node_identity(leaf)
            .map_err(|_| Status::unauthenticated("not a node certificate"))?;
        // Revocation: only the node's current certificate serial is accepted.
        let ok: Option<bool> = sqlx::query_scalar(
            "SELECT cert_serial = $2 FROM nodes WHERE id = $1 AND status <> 'revoked'",
        )
        .bind(node)
        .bind(&serial)
        .fetch_optional(&state.db)
        .await
        .map_err(internal)?;
        if ok != Some(true) {
            return Err(revoked());
        }

        let mut inbound = req.into_inner();
        let hello = match tokio::time::timeout(HELLO_TIMEOUT, inbound.message()).await {
            Ok(Ok(Some(pb::NodeMessage {
                msg: Some(In::Hello(h)),
            }))) => h,
            _ => return Err(Status::invalid_argument("expected Hello")),
        };
        let proto = pb::PROTOCOL;
        if hello.protocol != proto && hello.protocol + 1 != proto {
            return Err(Status::failed_precondition(format!(
                "protocol {} is not supported (server speaks {proto}); upgrade modula-node",
                hello.protocol
            )));
        }
        let info = hello.info.clone().unwrap_or_default();
        let location =
            crate::locations::resolve(&info.cloud_provider, &info.cloud_region, &info.edge_colo);
        // Guarded again: a revoke (or renewal) may have landed since the
        // check above.
        let hello_ok = sqlx::query(
            "UPDATE nodes SET status = 'online', node_version = $2, protocol = $3, runtime = $4,
                    applied_generation = $5, info = $6, last_seen_at = now(),
                    location = CASE WHEN location_pinned THEN location ELSE coalesce($7, location) END,
                    public_ip = coalesce($8, public_ip)
             WHERE id = $1 AND status <> 'revoked' AND cert_serial = $9",
        )
        .bind(node)
        .bind(hello.node_version.chars().take(64).collect::<String>())
        .bind(hello.protocol as i32)
        .bind(info.runtime_version.chars().take(128).collect::<String>())
        .bind(hello.applied_generation as i64)
        .bind(serde_json::json!({
            "hostname": info.hostname.chars().take(255).collect::<String>(),
            "os": info.os.chars().take(64).collect::<String>(),
            "arch": info.arch.chars().take(32).collect::<String>(),
            "cpus": info.cpus,
            "memoryBytes": info.memory_bytes,
            "capabilities": hello.capabilities.iter().take(32).map(|c| c.chars().take(64).collect::<String>()).collect::<Vec<_>>(),
            "cloudProvider": info.cloud_provider.chars().take(32).collect::<String>(),
            "cloudRegion": info.cloud_region.chars().take(64).collect::<String>(),
            "edgeColo": info.edge_colo.chars().take(8).collect::<String>(),
        }))
        .bind(location.map(|l| l.id))
        .bind(info.public_ip.parse::<std::net::Ipv4Addr>().ok().map(|ip| ip.to_string()))
        .bind(&serial)
        .execute(&state.db)
        .await
        .map_err(internal)?
        .rows_affected()
            == 1;
        if !hello_ok {
            return Err(revoked());
        }

        let (tx, rx) = mpsc::channel(256);
        let reg = state.hub.register(node, tx.clone());
        // A revoke that committed between the update above and `register`
        // disconnected nothing; drop this stream ourselves.
        match current(&state, node, &serial).await {
            Ok(true) => {}
            r => {
                state.hub.unregister(node, reg.conn_id);
                return Err(r.map_or_else(internal, |_| revoked()));
            }
        }
        let ack = pb::ServerMessage {
            msg: Some(Out::HelloAck(pb::HelloAck {
                protocol: proto,
                node_id: node.to_string(),
            })),
        };
        let _ = tx.send(Ok(ack)).await;
        tracing::info!(%node, "node attached");

        // Pusher: a full snapshot now, on every kick, and periodically.
        {
            let (state, tx, kick, cancel) = (
                state.clone(),
                tx.clone(),
                reg.kick.clone(),
                reg.cancel.clone(),
            );
            tokio::spawn(async move {
                loop {
                    match deploy::desired::build(&state, node).await {
                        Ok(ds) => {
                            let msg = pb::ServerMessage {
                                msg: Some(Out::Desired(ds)),
                            };
                            if tx.send(Ok(msg)).await.is_err() {
                                break;
                            }
                        }
                        Err(e) => tracing::error!(%node, error = ?e, "cannot build desired state"),
                    }
                    tokio::select! {
                        _ = kick.notified() => {}
                        _ = cancel.cancelled() => break,
                        _ = tokio::time::sleep(RESYNC_EVERY) => {}
                    }
                }
            });
        }

        // Reader: everything the node sends after Hello.
        {
            let (state, tx, cancel, conn_id) = (state.clone(), tx, reg.cancel, reg.conn_id);
            tokio::spawn(async move {
                let mut limits = Limits::new();
                loop {
                    let msg = tokio::select! {
                        m = inbound.message() => m,
                        _ = cancel.cancelled() => break,
                    };
                    match msg {
                        Ok(Some(pb::NodeMessage { msg: Some(m) })) => {
                            if !limits.admit(&m) {
                                limits.warn(node);
                                continue;
                            }
                            match handle(&state, node, &serial, &tx, m).await {
                                Ok(Flow::Continue) => {}
                                Ok(Flow::Drop) => {
                                    tracing::warn!(%node, "node is revoked; dropping its stream");
                                    break;
                                }
                                Err(e) => tracing::warn!(%node, error = ?e, "node message failed"),
                            }
                        }
                        Ok(Some(_)) => {}
                        Ok(None) | Err(_) => break,
                    }
                }
                if state.hub.unregister(node, conn_id) {
                    let _ = sqlx::query(
                        "UPDATE nodes SET status = 'offline' WHERE id = $1 AND status = 'online'",
                    )
                    .bind(node)
                    .execute(&state.db)
                    .await;
                    tracing::info!(%node, "node detached");
                }
                // Tunnels through this stream are gone with it.
                state.tunnels.node_gone(node);
            });
        }

        // The node reconnects to whichever server comes back up.
        let out = ReceiverStream::new(rx).take_until(self.state.shutdown.clone().cancelled_owned());
        Ok(Response::new(Box::pin(out)))
    }
}

fn revoked() -> Status {
    Status::unauthenticated("node is revoked or its certificate was replaced")
}

/// Whether `serial` is still the certificate of a node that is not revoked.
async fn current(state: &AppState, node: Id, serial: &str) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM nodes WHERE id = $1 AND status <> 'revoked' AND cert_serial = $2)",
    )
    .bind(node)
    .bind(serial)
    .fetch_one(&state.db)
    .await
}

/// What the reader does after a message.
enum Flow {
    Continue,
    /// The node was revoked under this stream.
    Drop,
}

/// Messages per second (and burst) a stream may send of the kinds that can
/// be dropped: logs, metrics and deployment events. A node over it loses
/// them; heartbeats, reports and replies are never limited.
const MSG_RATE: f64 = 200.0;
const MSG_BURST: f64 = 1000.0;
/// Shipped log bytes per second (and burst) per stream.
const LOG_BYTES_RATE: f64 = 4.0 * 1024.0 * 1024.0;
const LOG_BYTES_BURST: f64 = 8.0 * 1024.0 * 1024.0;
const WARN_EVERY: Duration = Duration::from_secs(10);

struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    at: std::time::Instant,
}

impl Bucket {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            rate,
            burst,
            at: std::time::Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.burst);
        self.at = now;
    }
}

/// Per-stream token buckets for what a node sends.
struct Limits {
    msgs: Bucket,
    log_bytes: Bucket,
    dropped: u64,
    warned: Option<std::time::Instant>,
}

impl Limits {
    fn new() -> Self {
        Self {
            msgs: Bucket::new(MSG_RATE, MSG_BURST),
            log_bytes: Bucket::new(LOG_BYTES_RATE, LOG_BYTES_BURST),
            dropped: 0,
            warned: None,
        }
    }

    /// False when `m` should be dropped.
    fn admit(&mut self, m: &In) -> bool {
        let bytes = match m {
            In::LogBatch(b) => b
                .lines
                .iter()
                .map(|l| l.text.len() + l.deployment_id.len())
                .sum::<usize>() as f64,
            In::Metrics(_) | In::Event(_) => 0.0,
            _ => return true,
        };
        self.msgs.refill();
        self.log_bytes.refill();
        if self.msgs.tokens < 1.0 || self.log_bytes.tokens < bytes.min(LOG_BYTES_BURST) {
            self.dropped += 1;
            return false;
        }
        self.msgs.tokens -= 1.0;
        self.log_bytes.tokens -= bytes.min(LOG_BYTES_BURST);
        true
    }

    /// At most one warning per [`WARN_EVERY`], with the count since the last.
    fn warn(&mut self, node: Id) {
        if self.warned.is_some_and(|t| t.elapsed() < WARN_EVERY) {
            return;
        }
        tracing::warn!(%node, dropped = self.dropped, "node is over its message rate; dropping");
        self.dropped = 0;
        self.warned = Some(std::time::Instant::now());
    }
}

/// Renew only once less than this much lifetime is left: a third of it, as
/// the node asks (its `RENEW_BEFORE`), plus a margin for clock skew.
const RENEW_WITHIN: time::Duration =
    time::Duration::seconds(crate::pki::NODE_CERT_TTL.whole_seconds() / 3 + 3600);

async fn handle(
    state: &AppState,
    node: Id,
    serial: &str,
    tx: &crate::nodes::hub::Outbound,
    msg: In,
) -> anyhow::Result<Flow> {
    match msg {
        In::Hello(_) => {}
        In::Heartbeat(_) => {
            let r = sqlx::query(
                "UPDATE nodes SET last_seen_at = now() WHERE id = $1 AND status <> 'revoked'",
            )
            .bind(node)
            .execute(&state.db)
            .await?;
            if r.rows_affected() == 0 {
                return Ok(Flow::Drop);
            }
        }
        In::Metrics(r) => crate::metrics::record(&state.db, node, &r).await?,
        In::LogBatch(b) => crate::logs::record(&state.db, node, &b).await?,
        In::Report(r) => {
            let r = sqlx::query(
                "UPDATE nodes SET applied_generation = GREATEST(applied_generation, $2), last_seen_at = now()
                 WHERE id = $1 AND status <> 'revoked'",
            )
            .bind(node)
            .bind(r.generation as i64)
            .execute(&state.db)
            .await?;
            if r.rows_affected() == 0 {
                return Ok(Flow::Drop);
            }
        }
        In::Event(ev) => {
            let kick = deploy::state::apply_event(&state.db, node, &ev)
                .await
                .map_err(|e| anyhow::anyhow!("{e:?}"))?;
            state.hub.kick_all(&kick);
        }
        In::Log(chunk) => {
            if let Some(request_id) = state.hub.deliver_logs(node, chunk) {
                let cancel = pb::ServerMessage {
                    msg: Some(Out::LogCancel(pb::LogCancel { request_id })),
                };
                let _ = tx.try_send(Ok(cancel));
            }
        }
        In::BackupResult(r) => crate::backup::on_result(state, node, r).await?,
        In::DbResult(r) => state.hub.deliver_db(node, r),
        In::UpgradeStatus(s) => crate::updates::rollout::on_status(state, node, s).await?,
        In::AccessStatus(s) => state.private.deliver(node, s),
        // Never waits: see crate::tunnel on flow control.
        m @ (In::TunnelOpened(_) | In::TunnelData(_) | In::TunnelClose(_)) => {
            if let Some(reply) = state.tunnels.from_node(node, m) {
                let _ = tx.try_send(Ok(reply));
            }
        }
        In::Renew(r) => {
            let pki = state.pki().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
            let issued = pki.issue_node_cert(node, &r.csr_der)?;
            // Only for this stream's (current) certificate, and only when it
            // is due: otherwise no reply, and the node asks again next connect.
            let r = sqlx::query(
                "UPDATE nodes SET cert_serial = $2, cert_expires_at = $3
                 WHERE id = $1 AND status <> 'revoked' AND cert_serial = $4 AND cert_expires_at < $5",
            )
            .bind(node)
            .bind(&issued.serial_hex)
            .bind(issued.not_after)
            .bind(serial)
            .bind(time::OffsetDateTime::now_utc() + RENEW_WITHIN)
            .execute(&state.db)
            .await?;
            if r.rows_affected() == 0 {
                tracing::warn!(%node, "ignoring a certificate renewal that is not due");
                return Ok(Flow::Continue);
            }
            let msg = pb::ServerMessage {
                msg: Some(Out::Renewed(pb::RenewResponse {
                    cert_der: issued.der,
                })),
            };
            let _ = tx.send(Ok(msg)).await;
            tracing::info!(%node, "node certificate renewed");
        }
    }
    Ok(Flow::Continue)
}
