//! `modula-node join`: generate a key pair, prove possession of a single-use
//! join token, and receive a node certificate. The private key never leaves
//! this machine; the server is trusted only via the pinned root fingerprint.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use modula_proto::node::v1 as pb;
use pb::node_client::NodeClient;
use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use uuid::Uuid;

use crate::identity::{Identity, Meta, pem};
use crate::tls::{Fingerprint, PinnedRoot};

pub struct JoinArgs<'a> {
    pub server: &'a str,
    pub token: &'a str,
    pub fingerprint: &'a str,
    pub hostname: String,
    pub dir: &'a Path,
}

/// A channel to the control plane that trusts only the pinned root. When
/// `identity` is given, it is presented as the client certificate (mTLS).
pub async fn connect(
    server: &str,
    pin: Fingerprint,
    identity: Option<&Identity>,
) -> anyhow::Result<Channel> {
    let mut tls = ClientTlsConfig::new();
    if let Some(id) = identity {
        tls = tls.identity(tonic::transport::Identity::from_pem(
            &id.cert_pem,
            &id.key_pem,
        ));
    }
    let channel = Endpoint::from_shared(server.to_owned())
        .context("invalid server URL")?
        .connect_timeout(Duration::from_secs(10))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .keep_alive_timeout(Duration::from_secs(10))
        .keep_alive_while_idle(true)
        .tls_config_with_verifier(tls, PinnedRoot::new(pin))?
        .connect()
        .await
        .with_context(|| format!("cannot reach {server}"))?;
    Ok(channel)
}

pub async fn join(args: JoinArgs<'_>) -> anyhow::Result<Identity> {
    if Identity::exists(args.dir) {
        bail!(
            "{} already holds a node identity; this server has joined before",
            args.dir.display()
        );
    }
    if !args.server.starts_with("https://") {
        bail!("--server must be an https:// URL");
    }
    let pin: Fingerprint = args.fingerprint.parse()?;

    let key = KeyPair::generate().context("cannot generate a key pair")?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "modula-node");
    params.distinguished_name = dn;
    let csr = params.serialize_request(&key)?;

    let channel = connect(args.server, pin, None).await?;
    let resp = NodeClient::new(channel)
        .enroll(pb::EnrollRequest {
            join_token: args.token.to_owned(),
            csr_der: csr.der().to_vec(),
            hostname: args.hostname,
            node_version: crate::VERSION.to_owned(),
        })
        .await
        .map_err(|s| anyhow::anyhow!("the server refused to enroll this node: {}", s.message()))?
        .into_inner();

    if Fingerprint::of(&resp.root_der) != pin {
        bail!("the server returned a different root certificate than the pinned one");
    }
    let node_id: Uuid = resp
        .node_id
        .parse()
        .context("server sent an invalid node id")?;
    let identity = Identity {
        dir: args.dir.to_owned(),
        meta: Meta {
            node_id,
            server: args.server.to_owned(),
            fingerprint: pin.to_string(),
        },
        cert_pem: format!(
            "{}{}",
            pem("CERTIFICATE", &resp.cert_der),
            pem("CERTIFICATE", &resp.intermediate_der)
        ),
        key_pem: key.serialize_pem(),
    };
    identity.save()?;
    Ok(identity)
}

/// A fresh key and CSR for certificate renewal.
pub fn renewal_request() -> anyhow::Result<(KeyPair, Vec<u8>)> {
    let key = KeyPair::generate()?;
    let csr = CertificateParams::new(Vec::<String>::new())?.serialize_request(&key)?;
    Ok((key, csr.der().to_vec()))
}
