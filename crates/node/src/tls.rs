//! Server certificate verification pinned to the control plane's root CA.
//!
//! A join command carries `sha256:<hex>` of the root certificate. The server
//! sends its chain including the root; we accept it only if one certificate in
//! the chain has exactly that fingerprint *and* the leaf chains to it under
//! normal WebPKI rules (names, validity, usages). No system roots are trusted.

use std::sync::Arc;

use anyhow::{Context, bail};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, RootCertStore, SignatureScheme};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub fn of(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }
}

impl std::str::FromStr for Fingerprint {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        let hex = s
            .strip_prefix("sha256:")
            .context("fingerprint must start with sha256:")?;
        let bytes = hex::decode(hex).context("fingerprint is not hex")?;
        let Ok(arr) = <[u8; 32]>::try_from(bytes.as_slice()) else {
            bail!("fingerprint must be 32 bytes");
        };
        Ok(Self(arr))
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sha256:{}", hex::encode(self.0))
    }
}

#[derive(Debug)]
pub struct PinnedRoot {
    pin: Fingerprint,
    provider: Arc<CryptoProvider>,
}

impl PinnedRoot {
    pub fn new(pin: Fingerprint) -> Arc<Self> {
        Arc::new(Self {
            pin,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        })
    }

    fn algorithms(&self) -> &WebPkiSupportedAlgorithms {
        &self.provider.signature_verification_algorithms
    }
}

impl ServerCertVerifier for PinnedRoot {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let root = intermediates
            .iter()
            .find(|c| Fingerprint::of(c) == self.pin)
            .ok_or(rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer,
            ))?;
        let mut roots = RootCertStore::empty();
        roots.add(root.clone().into_owned())?;
        let webpki =
            WebPkiServerVerifier::builder_with_provider(Arc::new(roots), self.provider.clone())
                .build()
                .map_err(|e| rustls::Error::General(e.to_string()))?;
        let rest: Vec<CertificateDer<'_>> = intermediates
            .iter()
            .filter(|c| *c != root)
            .cloned()
            .collect();
        webpki.verify_server_cert(end_entity, &rest, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, self.algorithms())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, self.algorithms())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms().supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_parsing() {
        let f = Fingerprint::of(b"root");
        let s = f.to_string();
        assert_eq!(s.parse::<Fingerprint>().unwrap(), f);
        assert!("md5:00".parse::<Fingerprint>().is_err());
        assert!("sha256:zz".parse::<Fingerprint>().is_err());
        assert!("sha256:00".parse::<Fingerprint>().is_err());
    }
}
