//! A minimal HTTP/1.1 client for Caddy's admin API, over a unix socket or
//! loopback TCP. One request per connection; no redirects, no TLS.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_RESPONSE: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminAddr {
    Unix(PathBuf),
    Tcp(String),
}

impl AdminAddr {
    /// `unix:/run/caddy/admin.sock` or `localhost:2019`.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        match s.strip_prefix("unix:") {
            Some(path) if path.starts_with('/') => Ok(Self::Unix(path.into())),
            Some(_) => bail!("unix admin socket must be an absolute path"),
            None if s.contains(':') && !s.contains('/') => Ok(Self::Tcp(s.to_owned())),
            None => bail!("admin address must be unix:/path or host:port"),
        }
    }

    /// How the same address is written in Caddy's own `admin.listen`.
    pub fn caddy_listen(&self) -> String {
        match self {
            Self::Unix(p) => format!("unix/{}", p.display()),
            Self::Tcp(a) => a.clone(),
        }
    }

    fn host_header(&self) -> &str {
        match self {
            Self::Unix(_) => "localhost",
            Self::Tcp(a) => a,
        }
    }
}

pub struct Response {
    pub status: u16,
    pub body: String,
}

pub async fn post_json(addr: &AdminAddr, path: &str, body: &[u8]) -> anyhow::Result<Response> {
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        addr.host_header(),
        body.len()
    );
    send(addr, head, body).await
}

pub async fn get(addr: &AdminAddr, path: &str) -> anyhow::Result<Response> {
    let head = format!(
        // HTTP/1.0 so the reply is never chunked; we only compare the body.
        "GET {path} HTTP/1.0\r\nHost: {}\r\n\r\n",
        addr.host_header()
    );
    send(addr, head, &[]).await
}

async fn send(addr: &AdminAddr, head: String, body: &[u8]) -> anyhow::Result<Response> {
    let fut = async {
        match addr {
            AdminAddr::Unix(p) => {
                let s = tokio::net::UnixStream::connect(p)
                    .await
                    .with_context(|| format!("cannot connect to {}", p.display()))?;
                exchange(s, head.as_bytes(), body).await
            }
            AdminAddr::Tcp(a) => {
                let s = tokio::net::TcpStream::connect(a)
                    .await
                    .with_context(|| format!("cannot connect to {a}"))?;
                exchange(s, head.as_bytes(), body).await
            }
        }
    };
    tokio::time::timeout(TIMEOUT, fut)
        .await
        .context("Caddy admin API timed out")?
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    head: &[u8],
    body: &[u8],
) -> anyhow::Result<Response> {
    s.write_all(head).await?;
    s.write_all(body).await?;
    let mut buf = Vec::new();
    (&mut s)
        .take(MAX_RESPONSE as u64)
        .read_to_end(&mut buf)
        .await?;
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .context("malformed response from Caddy")?;
    let body = text
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    Ok(Response { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admin_addresses() {
        assert_eq!(
            AdminAddr::parse("unix:/run/caddy/admin.sock")
                .unwrap()
                .caddy_listen(),
            "unix//run/caddy/admin.sock"
        );
        assert_eq!(
            AdminAddr::parse("localhost:2019").unwrap(),
            AdminAddr::Tcp("localhost:2019".into())
        );
        assert!(AdminAddr::parse("unix:relative").is_err());
        assert!(AdminAddr::parse("http://localhost:2019").is_err());
    }
}
