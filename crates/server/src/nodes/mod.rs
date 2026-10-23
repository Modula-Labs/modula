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
pub async fn create_join_token(
    db: &sqlx::PgPool,
    created_by: Option<uuid::Uuid>,
    ttl: time::Duration,
) -> AppResult<(secrecy::SecretString, time::OffsetDateTime)> {
    use modula_crypto::token::{self, prefix};
    let t = token::generate(prefix::NODE_JOIN)?;
    let expires = time::OffsetDateTime::now_utc() + ttl;
    sqlx::query(
        "INSERT INTO node_join_tokens (id, token_hash, created_by, expires_at) VALUES ($1, $2, $3, $4)",
    )
    .bind(uuid::Uuid::now_v7())
    .bind(t.hash.as_bytes())
    .bind(created_by)
    .bind(expires)
    .execute(db)
    .await?;
    Ok((t.plaintext, expires))
}

/// The command a new server runs to join this control plane.
pub fn join_command(cfg: &Config, token: &str, fingerprint: &str) -> String {
    let server = cfg.node_url.as_str().trim_end_matches('/');
    format!("modula-node join --server {server} --token {token} --ca {fingerprint}")
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
