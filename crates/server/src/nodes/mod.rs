//! The node endpoint: mTLS gRPC on its own port, never the browser API.

pub mod grpc;
pub mod hub;
pub mod tls;

use std::sync::Arc;
use std::time::Duration;

use modula_proto::node::v1::node_server::NodeServer;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;

pub use hub::Hub;

use crate::config::Config;
use crate::error::AppResult;
use crate::state::AppState;

/// Join tokens: `mcj_…`, single use, short-lived; only the hash is stored.
/// A token with a `workspace` enrolls a server private to that workspace;
/// without one (an instance admin's) the server is shared.
pub async fn create_join_token(
    db: impl sqlx::PgExecutor<'_>,
    created_by: Option<modula_id::Id>,
    workspace: Option<modula_id::Id>,
    ttl: time::Duration,
) -> AppResult<(modula_id::Id, secrecy::SecretString, time::OffsetDateTime)> {
    use modula_crypto::token::{self, prefix};
    let t = token::generate(prefix::NODE_JOIN)?;
    let id = modula_id::Id::new();
    let expires = time::OffsetDateTime::now_utc() + ttl;
    sqlx::query(
        "INSERT INTO node_join_tokens (id, token_hash, created_by, workspace_id, expires_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(t.hash.as_bytes())
    .bind(created_by)
    .bind(workspace)
    .bind(expires)
    .execute(db)
    .await?;
    Ok((id, t.plaintext, expires))
}

/// Revoke a node: it stops counting for placement, loses what ran on it,
/// and its certificate serial is tombstoned so that certificate can never
/// attach again (a revoked server re-joins with a new token). False when it
/// was already revoked. The caller disconnects it after committing.
pub async fn revoke(tx: &mut sqlx::PgConnection, node: modula_id::Id) -> AppResult<bool> {
    // Its peers drop it now (bumped before it leaves the mesh); its /24 is free again.
    crate::mesh::bump_peers(&mut *tx, node).await?;
    let r = sqlx::query(
        "UPDATE nodes SET status = 'revoked', revoked_at = now(), cert_serial = 'revoked:' || cert_serial,
                mesh_index = NULL, wg_public_key = NULL
         WHERE id = $1 AND status <> 'revoked'",
    )
    .bind(node)
    .execute(&mut *tx)
    .await?;
    if r.rows_affected() == 0 {
        return Ok(false);
    }
    crate::deploy::remove_live(&mut *tx, crate::deploy::LiveFilter::Node, node).await?;
    sqlx::query("UPDATE service_instances SET node_id = NULL WHERE node_id = $1")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    // Replicas it ran for deployments placed elsewhere stop being addressed.
    sqlx::query(
        "UPDATE deployment_replicas SET node_id = NULL, mesh_ip = NULL, status = 'failed', updated_at = now()
         WHERE node_id = $1",
    )
    .bind(node)
    .execute(&mut *tx)
    .await?;
    Ok(true)
}

/// The command a new server runs to join this control plane.
/// The bare `modula-node join` invocation, for a server that already has it.
pub fn node_join_command(cfg: &Config, token: &str, fingerprint: &str) -> String {
    let server = cfg.node_url.as_str().trim_end_matches('/');
    format!("modula-node join --server {server} --token {token} --ca {fingerprint}")
}

/// One line for a fresh server: fetch this instance's installer, which
/// installs Docker, Caddy and modula-node and then joins.
pub fn join_command(cfg: &Config, token: &str, fingerprint: &str) -> String {
    let server = cfg.node_url.as_str().trim_end_matches('/');
    format!(
        "curl -fsSL {}/api/install.sh | sudo bash -s -- join --server {server} --token {token} --ca {fingerprint}",
        cfg.public_origin()
    )
}

pub async fn serve(
    state: AppState,
    listener: TcpListener,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    let pki = state.pki().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let certs = tls::RotatingCert::new(pki, state.cfg.node_hosts(), provider)?;
    let config = tls::server_config(pki, certs.clone())?;
    tracing::info!(listen = %listener.local_addr()?, fingerprint = %pki.fingerprint(), "node endpoint listening");

    let rotation = {
        let (state, stop) = (state.clone(), stop.clone());
        tokio::spawn(async move {
            if let Ok(pki) = state.pki().await {
                tls::rotate(pki, certs, stop).await;
            }
        })
    };
    let result = Server::builder()
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
        .add_service(NodeServer::new(grpc::NodeService { state }))
        .serve_with_incoming_shutdown(
            tls::incoming(listener, config, stop.clone()),
            stop.cancelled_owned(),
        )
        .await;
    rotation.abort();
    result?;
    Ok(())
}
