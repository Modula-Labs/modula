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
        let token_id: Option<Id> = sqlx::query_scalar(
            "UPDATE node_join_tokens SET used_at = now()
             WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()
             RETURNING id",
        )
        .bind(token::hash(&req.join_token).as_bytes())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?;
        let Some(token_id) = token_id else {
            return Err(Status::permission_denied("invalid or expired join token"));
        };
        sqlx::query(
            "INSERT INTO nodes (id, name, cert_serial, cert_expires_at, node_version)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(node_id)
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
            return Err(Status::unauthenticated(
                "node is revoked or its certificate was replaced",
            ));
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
        sqlx::query(
            "UPDATE nodes SET status = 'online', node_version = $2, protocol = $3, runtime = $4,
                    applied_generation = $5, info = $6, last_seen_at = now(),
                    location = CASE WHEN location_pinned THEN location ELSE coalesce($7, location) END,
                    public_ip = coalesce($8, public_ip)
             WHERE id = $1",
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
        .execute(&state.db)
        .await
        .map_err(internal)?;

        let (tx, rx) = mpsc::channel(256);
        let reg = state.hub.register(node, tx.clone());
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
                loop {
                    let msg = tokio::select! {
                        m = inbound.message() => m,
                        _ = cancel.cancelled() => break,
                    };
                    match msg {
                        Ok(Some(pb::NodeMessage { msg: Some(m) })) => {
                            if let Err(e) = handle(&state, node, &tx, m).await {
                                tracing::warn!(%node, error = ?e, "node message failed");
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

async fn handle(
    state: &AppState,
    node: Id,
    tx: &crate::nodes::hub::Outbound,
    msg: In,
) -> anyhow::Result<()> {
    match msg {
        In::Hello(_) => {}
        In::Heartbeat(_) => {
            sqlx::query("UPDATE nodes SET last_seen_at = now() WHERE id = $1")
                .bind(node)
                .execute(&state.db)
                .await?;
        }
        In::Report(r) => {
            sqlx::query(
                "UPDATE nodes SET applied_generation = GREATEST(applied_generation, $2), last_seen_at = now()
                 WHERE id = $1",
            )
            .bind(node)
            .bind(r.generation as i64)
            .execute(&state.db)
            .await?;
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
            sqlx::query("UPDATE nodes SET cert_serial = $2, cert_expires_at = $3 WHERE id = $1 AND status <> 'revoked'")
                .bind(node)
                .bind(&issued.serial_hex)
                .bind(issued.not_after)
                .execute(&state.db)
                .await?;
            let msg = pb::ServerMessage {
                msg: Some(Out::Renewed(pb::RenewResponse {
                    cert_der: issued.der,
                })),
            };
            let _ = tx.send(Ok(msg)).await;
            tracing::info!(%node, "node certificate renewed");
        }
    }
    Ok(())
}
