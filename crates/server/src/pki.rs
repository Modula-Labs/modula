//! Internal PKI for nodes (modula-node).
//!
//! Root CA (10 y) → intermediate CA (5 y, path length 0) → short-lived leaf
//! certificates for nodes (client auth) and for the node listener (server auth).
//! CA private keys are sealed under the master key; only the intermediate key is
//! held in memory at runtime. Joining nodes pin the root's SHA-256 fingerprint.

use anyhow::{Context, anyhow, bail};
use modula_crypto::envelope::{FieldAad, INSTANCE_SCOPE};
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DistinguishedName,
    DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose, SanType, SerialNumber,
};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use crate::keyring::Keyring;

pub const NODE_CERT_TTL: Duration = Duration::days(30);
const SERVER_CERT_TTL: Duration = Duration::days(90);
const NODE_URI_PREFIX: &str = "modula://node/";

pub struct Pki {
    root_der: Vec<u8>,
    int_der: Vec<u8>,
    int_key: KeyPair,
}

pub struct Issued {
    pub der: Vec<u8>,
    pub serial_hex: String,
    pub not_after: OffsetDateTime,
}

fn aad(column: &str) -> FieldAad<'_> {
    FieldAad {
        scope: INSTANCE_SCOPE,
        table: "pki",
        column,
        row: "ca",
    }
}

fn random_serial() -> anyhow::Result<SerialNumber> {
    let mut b = modula_crypto::random_bytes::<16>()?;
    b[0] &= 0x7f; // positive INTEGER
    b[0] |= 0x01; // and never zero-padded away
    Ok(SerialNumber::from_slice(&b))
}

fn dn(cn: &str) -> DistinguishedName {
    let mut d = DistinguishedName::new();
    d.push(DnType::OrganizationName, "Modula Deploy");
    d.push(DnType::CommonName, cn);
    d
}

fn ca_params(cn: &str, ttl: Duration, path_len: Option<u8>) -> anyhow::Result<CertificateParams> {
    let mut p = CertificateParams::default();
    let now = OffsetDateTime::now_utc();
    p.not_before = now - Duration::minutes(5);
    p.not_after = now + ttl;
    p.distinguished_name = dn(cn);
    p.serial_number = Some(random_serial()?);
    p.is_ca = IsCa::Ca(match path_len {
        Some(n) => BasicConstraints::Constrained(n),
        None => BasicConstraints::Unconstrained,
    });
    p.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    p.use_authority_key_identifier_extension = true;
    Ok(p)
}

impl Pki {
    /// Load the CA from the database, creating it on first boot. Concurrent first
    /// boots race on a single-row insert; the loser reloads the winner's CA.
    pub async fn load_or_init(db: &PgPool, keys: &Keyring) -> anyhow::Result<Self> {
        if let Some(p) = Self::load(db, keys).await? {
            return Ok(p);
        }
        let root_key = KeyPair::generate()?;
        let root_params = ca_params("Modula Deploy Root CA", Duration::days(3650), None)?;
        let root_cert = root_params.self_signed(&root_key)?;
        let root_issuer = Issuer::new(root_params, &root_key);

        let int_key = KeyPair::generate()?;
        let int_params = ca_params("Modula Deploy Node CA", Duration::days(1825), Some(0))?;
        let int_cert = int_params.signed_by(&int_key, &root_issuer)?;

        let root_sealed = keys
            .seal(db, &root_key.serialize_der(), aad("root_key"))
            .await?;
        let int_sealed = keys
            .seal(db, &int_key.serialize_der(), aad("int_key"))
            .await?;
        sqlx::query(
            "INSERT INTO pki (id, root_cert, root_key, int_cert, int_key) VALUES (true, $1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(root_cert.der().as_ref())
        .bind(&root_sealed)
        .bind(int_cert.der().as_ref())
        .bind(&int_sealed)
        .execute(db)
        .await?;
        Self::load(db, keys)
            .await?
            .ok_or_else(|| anyhow!("PKI row missing after insert"))
    }

    async fn load(db: &PgPool, keys: &Keyring) -> anyhow::Result<Option<Self>> {
        let row: Option<(Vec<u8>, Vec<u8>, Vec<u8>)> =
            sqlx::query_as("SELECT root_cert, int_cert, int_key FROM pki WHERE id")
                .fetch_optional(db)
                .await?;
        let Some((root_der, int_der, int_sealed)) = row else {
            return Ok(None);
        };
        let int_pkcs8 = keys
            .open(db, &int_sealed, aad("int_key"))
            .await
            .map_err(|e| anyhow!("cannot unseal the node CA key: {e}"))?;
        let int_key = KeyPair::try_from(int_pkcs8.as_slice()).context("bad node CA key")?;
        Ok(Some(Self {
            root_der,
            int_der,
            int_key,
        }))
    }

    pub fn root_der(&self) -> &[u8] {
        &self.root_der
    }

    pub fn intermediate_der(&self) -> &[u8] {
        &self.int_der
    }

    /// What joining nodes pin: `sha256:<hex of the root certificate DER>`.
    pub fn fingerprint(&self) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(&self.root_der)))
    }

    fn issuer(&self) -> anyhow::Result<Issuer<'static, &KeyPair>> {
        Issuer::from_ca_cert_der(&self.int_der.clone().into(), &self.int_key)
            .context("bad node CA certificate")
    }

    /// Issue a node client certificate. Only the CSR's public key is used; every
    /// other field (names, usages, validity) is set here, never by the requester.
    pub fn issue_node_cert(&self, node_id: Uuid, csr_der: &[u8]) -> anyhow::Result<Issued> {
        let csr = CertificateSigningRequestParams::from_der(&csr_der.to_vec().into())
            .map_err(|e| anyhow!("invalid CSR: {e}"))?;
        let now = OffsetDateTime::now_utc();
        let mut p = CertificateParams::default();
        p.not_before = now - Duration::minutes(5);
        p.not_after = now + NODE_CERT_TTL;
        p.distinguished_name = dn(&format!("node {node_id}"));
        let serial = random_serial()?;
        let serial_hex = hex::encode(serial.as_ref());
        p.serial_number = Some(serial);
        p.subject_alt_names = vec![SanType::URI(
            format!("{NODE_URI_PREFIX}{node_id}").try_into()?,
        )];
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        p.use_authority_key_identifier_extension = true;
        let cert = p.signed_by(&csr.public_key, &self.issuer()?)?;
        Ok(Issued {
            der: cert.der().to_vec(),
            serial_hex,
            not_after: p.not_after,
        })
    }

    /// Server certificate for the node listener. Returns (chain PEM, key PEM);
    /// the chain includes the root so joining nodes can check their pin.
    pub fn issue_server_cert(&self, hosts: &[String]) -> anyhow::Result<(String, String)> {
        if hosts.is_empty() {
            bail!("no node host names configured");
        }
        let key = KeyPair::generate()?;
        let now = OffsetDateTime::now_utc();
        let mut p = CertificateParams::default();
        p.not_before = now - Duration::minutes(5);
        p.not_after = now + SERVER_CERT_TTL;
        p.distinguished_name = dn("modula-server node endpoint");
        p.serial_number = Some(random_serial()?);
        p.subject_alt_names = hosts
            .iter()
            .map(|h| match h.parse::<std::net::IpAddr>() {
                Ok(ip) => Ok(SanType::IpAddress(ip)),
                Err(_) => Ok(SanType::DnsName(h.clone().try_into()?)),
            })
            .collect::<Result<_, rcgen::Error>>()?;
        p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        p.use_authority_key_identifier_extension = true;
        let cert = p.signed_by(&key, &self.issuer()?)?;
        let chain = format!(
            "{}{}{}",
            cert.pem(),
            pem("CERTIFICATE", &self.int_der),
            pem("CERTIFICATE", &self.root_der)
        );
        Ok((chain, key.serialize_pem()))
    }

    pub fn root_pem(&self) -> String {
        pem("CERTIFICATE", &self.root_der)
    }
}

pub fn pem(label: &str, der: &[u8]) -> String {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// Node identity from a verified client certificate: (node id, serial hex).
pub fn node_identity(leaf_der: &[u8]) -> anyhow::Result<(Uuid, String)> {
    use x509_parser::prelude::*;
    let (_, cert) =
        X509Certificate::from_der(leaf_der).map_err(|e| anyhow!("bad client cert: {e}"))?;
    let serial_hex = hex::encode(cert.raw_serial());
    let san = cert
        .subject_alternative_name()
        .map_err(|e| anyhow!("bad SAN: {e}"))?
        .ok_or_else(|| anyhow!("client cert has no SAN"))?;
    let mut ids = san.value.general_names.iter().filter_map(|n| match n {
        GeneralName::URI(u) => u.strip_prefix(NODE_URI_PREFIX),
        _ => None,
    });
    let id = ids
        .next()
        .ok_or_else(|| anyhow!("client cert is not a node certificate"))?;
    if ids.next().is_some() {
        bail!("client cert names more than one node");
    }
    Ok((Uuid::parse_str(id)?, serial_hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pki() -> Pki {
        let root_key = KeyPair::generate().unwrap();
        let root_params = ca_params("root", Duration::days(1), None).unwrap();
        let root = root_params.self_signed(&root_key).unwrap();
        let int_key = KeyPair::generate().unwrap();
        let int = ca_params("int", Duration::days(1), Some(0))
            .unwrap()
            .signed_by(&int_key, &Issuer::new(root_params, &root_key))
            .unwrap();
        Pki {
            root_der: root.der().to_vec(),
            int_der: int.der().to_vec(),
            int_key,
        }
    }

    fn csr(names: Vec<String>) -> Vec<u8> {
        let key = KeyPair::generate().unwrap();
        CertificateParams::new(names)
            .unwrap()
            .serialize_request(&key)
            .unwrap()
            .der()
            .to_vec()
    }

    #[test]
    fn node_cert_ignores_requested_names() {
        let p = pki();
        let id = Uuid::now_v7();
        // The CSR asks for a server name and a different node URI; neither is honoured.
        let issued = p
            .issue_node_cert(
                id,
                &csr(vec![
                    "evil.example.com".into(),
                    format!("{NODE_URI_PREFIX}{}", Uuid::nil()),
                ]),
            )
            .unwrap();
        let (got, serial) = node_identity(&issued.der).unwrap();
        assert_eq!(got, id);
        assert_eq!(serial, issued.serial_hex);
        use x509_parser::prelude::*;
        let (_, cert) = X509Certificate::from_der(&issued.der).unwrap();
        let san = cert.subject_alternative_name().unwrap().unwrap();
        assert_eq!(san.value.general_names.len(), 1);
        let eku = cert.extended_key_usage().unwrap().unwrap().value;
        assert!(eku.client_auth && !eku.server_auth);
        assert!(!cert.is_ca());
    }

    #[test]
    fn rejects_garbage_csr_and_foreign_certs() {
        let p = pki();
        assert!(p.issue_node_cert(Uuid::now_v7(), b"not a csr").is_err());
        let (chain, _) = p
            .issue_server_cert(&["localhost".into(), "127.0.0.1".into()])
            .unwrap();
        assert_eq!(chain.matches("BEGIN CERTIFICATE").count(), 3);
        // A server cert has no node URI.
        let leaf_b64: String = chain
            .lines()
            .skip(1)
            .take_while(|l| !l.starts_with("-----"))
            .collect();
        use base64::Engine as _;
        let leaf = base64::engine::general_purpose::STANDARD
            .decode(leaf_b64)
            .unwrap();
        assert!(node_identity(&leaf).is_err());
    }

    #[test]
    fn fingerprint_shape() {
        let f = pki().fingerprint();
        assert!(f.starts_with("sha256:") && f.len() == 7 + 64);
    }
}
