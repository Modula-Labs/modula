//! The node endpoint: mTLS gRPC on its own port, never the browser API.

pub mod grpc;
pub mod hub;

use std::time::Duration;

use modula_proto::node::v1::node_server::NodeServer;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};

pub use hub::Hub;

use crate::state::AppState;

pub async fn serve(
    state: AppState,
    listener: TcpListener,
    stop: CancellationToken,
) -> anyhow::Result<()> {
    let pki = state.pki().await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let (chain, key) = pki.issue_server_cert(&state.cfg.node_hosts())?;
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(chain, key))
        // Enroll/GetCaBundle run before a node has a certificate; Attach
        // requires one and checks it itself.
        .client_ca_root(Certificate::from_pem(pki.root_pem()))
        .client_auth_optional(true);
    tracing::info!(listen = %listener.local_addr()?, fingerprint = %pki.fingerprint(), "node endpoint listening");
    Server::builder()
        .tls_config(tls)?
        .http2_keepalive_interval(Some(Duration::from_secs(20)))
        .http2_keepalive_timeout(Some(Duration::from_secs(10)))
        .add_service(NodeServer::new(grpc::NodeService { state }))
        .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop.cancelled_owned())
        .await?;
    Ok(())
}
