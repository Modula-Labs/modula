//! TLS for the node endpoint. A rustls acceptor (rather than a fixed tonic
//! TLS config) so the server certificate can rotate without a restart:
//! a background task reissues it well before it expires.
//!
//! Client certificates are optional at the TLS layer (Enroll runs before a
//! node has one); when presented they must chain to our root, and Attach
//! additionally requires one and checks its serial against the database.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use rustls::{RootCertStore, ServerConfig};
use time::OffsetDateTime;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

use crate::pki::Pki;

/// Reissue when less than this much lifetime is left.
const RENEW_BEFORE: time::Duration = time::Duration::days(30);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Current {
    key: Arc<CertifiedKey>,
    not_after: OffsetDateTime,
}

#[derive(Debug)]
pub struct RotatingCert {
    hosts: Vec<String>,
    provider: Arc<CryptoProvider>,
    current: RwLock<Current>,
}

impl RotatingCert {
    fn issue(pki: &Pki, hosts: &[String], provider: &CryptoProvider) -> anyhow::Result<Current> {
        let cert = pki.issue_server_cert(hosts)?;
        let chain: Vec<CertificateDer<'static>> = cert
            .chain_der
            .into_iter()
            .map(CertificateDer::from)
            .collect();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_der));
        let signing = provider
            .key_provider
            .load_private_key(key)
            .context("unusable server key")?;
        Ok(Current {
            key: Arc::new(CertifiedKey::new(chain, signing)),
            not_after: cert.not_after,
        })
    }

    pub fn new(
        pki: &Pki,
        hosts: Vec<String>,
        provider: Arc<CryptoProvider>,
    ) -> anyhow::Result<Arc<Self>> {
        let current = Self::issue(pki, &hosts, &provider)?;
        Ok(Arc::new(Self {
            hosts,
            provider,
            current: RwLock::new(current),
        }))
    }

    /// Reissue if the current certificate is close to expiry. Returns whether it did.
    pub fn refresh_if_due(&self, pki: &Pki, now: OffsetDateTime) -> anyhow::Result<bool> {
        let due = self.current.read().expect("cert lock").not_after - now < RENEW_BEFORE;
        if due {
            let fresh = Self::issue(pki, &self.hosts, &self.provider)?;
            *self.current.write().expect("cert lock") = fresh;
        }
        Ok(due)
    }

    pub fn not_after(&self) -> OffsetDateTime {
        self.current.read().expect("cert lock").not_after
    }
}

impl ResolvesServerCert for RotatingCert {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current.read().expect("cert lock").key.clone())
    }
}

pub fn server_config(pki: &Pki, certs: Arc<RotatingCert>) -> anyhow::Result<Arc<ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.add(CertificateDer::from(pki.root_der().to_vec()))?;
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .allow_unauthenticated()
        .build()?;
    let mut cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(certs);
    cfg.alpn_protocols = vec![b"h2".to_vec()];
    Ok(Arc::new(cfg))
}

/// Accept TCP connections and complete TLS handshakes concurrently, so one
/// slow or hostile client can't hold up the others.
pub fn incoming(
    listener: TcpListener,
    cfg: Arc<ServerConfig>,
    stop: CancellationToken,
) -> ReceiverStream<Result<TlsStream<TcpStream>, std::io::Error>> {
    let (tx, rx) = mpsc::channel(64);
    let acceptor = TlsAcceptor::from(cfg);
    tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                a = listener.accept() => a,
                _ = stop.cancelled() => break,
            };
            let (tcp, peer) = match accepted {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let (acceptor, tx) = (acceptor.clone(), tx.clone());
            tokio::spawn(async move {
                match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
                    Ok(Ok(tls)) => {
                        let _ = tx.send(Ok(tls)).await;
                    }
                    Ok(Err(e)) => tracing::debug!(%peer, error = %e, "TLS handshake failed"),
                    Err(_) => tracing::debug!(%peer, "TLS handshake timed out"),
                }
            });
        }
    });
    ReceiverStream::new(rx)
}

/// Keep the endpoint certificate fresh for as long as the server runs.
pub async fn rotate(pki: &Pki, certs: Arc<RotatingCert>, stop: CancellationToken) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(CHECK_EVERY) => {}
        }
        match certs.refresh_if_due(pki, OffsetDateTime::now_utc()) {
            Ok(true) => {
                tracing::info!(not_after = %certs.not_after(), "node endpoint certificate renewed")
            }
            Ok(false) => {}
            Err(e) => tracing::error!(error = ?e, "cannot renew the node endpoint certificate"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_only_when_close_to_expiry() {
        let pki = crate::pki::test_pki();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let certs = RotatingCert::new(&pki, vec!["localhost".into()], provider).unwrap();
        let first = certs.not_after();
        assert!(
            !certs
                .refresh_if_due(&pki, OffsetDateTime::now_utc())
                .unwrap()
        );
        assert_eq!(certs.not_after(), first);
        // 75 days later the 90-day certificate has under 30 days left.
        let later = OffsetDateTime::now_utc() + time::Duration::days(75);
        assert!(certs.refresh_if_due(&pki, later).unwrap());
        assert!(certs.not_after() >= first);
        assert!(server_config(&pki, certs).is_ok());
    }
}
