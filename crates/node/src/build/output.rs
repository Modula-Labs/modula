//! Build output for static sites hosted elsewhere (`Build.output_dir`): the
//! output directory, packed from the built image as a gzipped tarball, goes
//! to the control plane, which publishes it. The node never talks to the
//! hosting provider and holds no credentials for it: only the upload token
//! bound to this deployment from its desired state.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

/// The archive's hex SHA-256 (reported with BUILT, checked by the server).
pub async fn sha256(path: &Path) -> anyhow::Result<String> {
    let mut f = tokio::fs::File::open(path).await?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}

/// PUTs the archive to `url` with `auth` as the Authorization header.
pub async fn upload(url: &str, auth: &str, path: &Path) -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .user_agent(concat!("modula-node/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let file = tokio::fs::File::open(path).await?;
    let len = file.metadata().await?.len();
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let resp = http
        .put(url)
        .header(reqwest::header::AUTHORIZATION, auth)
        .header(reqwest::header::CONTENT_TYPE, "application/gzip")
        .header(reqwest::header::CONTENT_LENGTH, len)
        .body(body)
        .send()
        .await
        .context("cannot reach the control plane")?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text: String = resp
            .text()
            .await
            .unwrap_or_default()
            .chars()
            .take(300)
            .collect();
        bail!("the control plane refused the build output: HTTP {status} {text}");
    }
    Ok(())
}

/// Writes a gzipped tarball of `files` (path inside `top`, contents) with one
/// top-level directory `top`, the layout `docker cp` produces (FakeBuilder).
pub fn pack(dest: &Path, top: &str, files: &[(&str, &[u8])]) -> anyhow::Result<()> {
    let f = std::fs::File::create(dest)?;
    let gz = flate2::write::GzEncoder::new(f, flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    for (path, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, format!("{top}/{path}"), *data)?;
    }
    tar.into_inner()?.finish()?;
    Ok(())
}
