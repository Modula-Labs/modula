//! What a joined node keeps on disk (default `/var/lib/modula-node`):
//!
//! - `node.json`: node id, server URL, pinned root fingerprint
//! - `node.key`:  private key (mode 0600, never leaves the machine)
//! - `node.crt`:  certificate chain (leaf + intermediate)

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::tls::Fingerprint;

const META: &str = "node.json";
const KEY: &str = "node.key";
const CERT: &str = "node.crt";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub node_id: Uuid,
    pub server: String,
    pub fingerprint: String,
}

#[derive(Clone)]
pub struct Identity {
    pub dir: PathBuf,
    pub meta: Meta,
    pub cert_pem: String,
    pub key_pem: String,
}

impl Identity {
    pub fn exists(dir: &Path) -> bool {
        dir.join(META).exists()
    }

    pub fn load(dir: &Path) -> anyhow::Result<Self> {
        let meta: Meta =
            serde_json::from_slice(&std::fs::read(dir.join(META)).with_context(|| {
                format!("{} is not joined; run `modula-node join`", dir.display())
            })?)
            .context("node.json is malformed")?;
        check_private(&dir.join(KEY))?;
        Ok(Self {
            dir: dir.to_owned(),
            cert_pem: std::fs::read_to_string(dir.join(CERT)).context("cannot read node.crt")?,
            key_pem: std::fs::read_to_string(dir.join(KEY)).context("cannot read node.key")?,
            meta,
        })
    }

    pub fn fingerprint(&self) -> anyhow::Result<Fingerprint> {
        self.meta.fingerprint.parse()
    }

    /// Write everything atomically (temp file + rename), key first.
    pub fn save(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        write_atomic(&self.dir.join(KEY), self.key_pem.as_bytes(), 0o600)?;
        write_atomic(&self.dir.join(CERT), self.cert_pem.as_bytes(), 0o644)?;
        write_atomic(
            &self.dir.join(META),
            &serde_json::to_vec_pretty(&self.meta)?,
            0o644,
        )?;
        Ok(())
    }

    /// When the leaf certificate expires.
    pub fn cert_not_after(&self) -> anyhow::Result<std::time::SystemTime> {
        let der = pem_blocks(&self.cert_pem, "CERTIFICATE")
            .into_iter()
            .next()
            .context("node.crt has no certificate")?;
        let (_, cert) = x509_parser::parse_x509_certificate(&der)
            .context("node.crt is not a valid certificate")?;
        let secs = cert.validity().not_after.timestamp();
        Ok(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs.max(0) as u64))
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

/// DER bodies of every `label` block in `pem`.
pub fn pem_blocks(pem: &str, label: &str) -> Vec<Vec<u8>> {
    use base64::Engine as _;
    let (begin, end) = (
        format!("-----BEGIN {label}-----"),
        format!("-----END {label}-----"),
    );
    let mut out = Vec::new();
    let mut body: Option<String> = None;
    for line in pem.lines().map(str::trim) {
        if line == begin {
            body = Some(String::new());
        } else if line == end {
            if let Some(b) = body.take()
                && let Ok(der) = base64::engine::general_purpose::STANDARD.decode(b)
            {
                out.push(der);
            }
        } else if let Some(b) = body.as_mut() {
            b.push_str(line);
        }
    }
    out
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(mode);
    }
    let mut f = opts
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn check_private(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .with_context(|| format!("cannot read {}", path.display()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            bail!(
                "{} is readable by other users (mode {:o}); run chmod 600",
                path.display(),
                mode & 0o777
            );
        }
    }
    Ok(())
}
