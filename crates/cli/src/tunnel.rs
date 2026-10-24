//! The local end of a tunnel: a TCP listener on 127.0.0.1 whose every
//! connection is carried over its own WebSocket to
//! `/api/service-instances/{id}/tunnel` (binary frames = raw TCP bytes).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, bail};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use url::Url;

use crate::api::{ApiError, USER_AGENT, install_crypto};
use crate::config::Login;

/// Read size for the local socket; the server relays at most 64 KiB per chunk.
const CHUNK: usize = 64 * 1024;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub type Ws = WebSocketStream<Box<dyn Io>>;

/// Where tunnels for one service instance go.
#[derive(Clone)]
pub struct Target {
    url: Url,
    token: String,
    /// Cloudflare Access headers (private mode).
    access: Vec<(&'static str, String)>,
    tls: Option<Arc<rustls::ClientConfig>>,
}

/// `https://host/` → `wss://host/api/service-instances/<id>/tunnel[?port=]`.
pub fn tunnel_url(base: &Url, instance: &str, port: Option<u16>) -> anyhow::Result<Url> {
    let mut url = ws_url(base, &format!("api/service-instances/{instance}/tunnel"))?;
    if let Some(p) = port {
        url.query_pairs_mut().append_pair("port", &p.to_string());
    }
    Ok(url)
}

/// `https://host/` + `api/…` → `wss://host/api/…` (`ws://` for http).
pub fn ws_url(base: &Url, path: &str) -> anyhow::Result<Url> {
    let mut url = base.join(path)?;
    let scheme = match base.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => bail!("unsupported URL scheme {other}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("cannot use {scheme} with {base}"))?;
    Ok(url)
}

impl Target {
    pub fn new(login: &Login, instance: &str, port: Option<u16>) -> anyhow::Result<Self> {
        Self::at(login, tunnel_url(&login.url, instance, port)?)
    }

    /// Any WebSocket endpoint of the API (`ws://` or `wss://`).
    pub fn at(login: &Login, url: Url) -> anyhow::Result<Self> {
        crate::config::check_transport(&login.url)?;
        let tls = if url.scheme() == "wss" {
            install_crypto();
            use rustls_platform_verifier::ConfigVerifierExt;
            Some(Arc::new(
                rustls::ClientConfig::with_platform_verifier()
                    .context("cannot load the system's trusted certificates")?,
            ))
        } else {
            None
        };
        Ok(Self {
            url,
            token: login.token.clone(),
            access: login
                .access
                .as_ref()
                .map(|a| a.headers())
                .unwrap_or_default(),
            tls,
        })
    }

    /// Open the WebSocket (for a tunnel: one TCP connection to the service).
    pub async fn open(&self) -> anyhow::Result<Ws> {
        let host = self.url.host_str().context("URL has no host")?.to_owned();
        let port = self
            .url
            .port_or_known_default()
            .context("URL has no port")?;
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("cannot reach {host}:{port}"))?;
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Io> = match &self.tls {
            Some(cfg) => {
                let name = rustls::pki_types::ServerName::try_from(host.clone())
                    .with_context(|| format!("invalid host name {host}"))?;
                let tls = tokio_rustls::TlsConnector::from(cfg.clone())
                    .connect(name, tcp)
                    .await
                    .with_context(|| format!("TLS with {host} failed"))?;
                Box::new(tls)
            }
            None => Box::new(tcp),
        };
        let mut req = self.url.as_str().into_client_request()?;
        let h = req.headers_mut();
        h.insert(
            http::header::AUTHORIZATION,
            format!("Bearer {}", self.token)
                .parse()
                .context("the API token contains invalid characters")?,
        );
        h.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static(USER_AGENT),
        );
        for (k, v) in &self.access {
            h.insert(
                *k,
                v.parse()
                    .context("the Cloudflare Access credentials contain invalid characters")?,
            );
        }
        match tokio_tungstenite::client_async(req, io).await {
            Ok((ws, _)) => Ok(ws),
            // The API's own error (no running deployment, not allowed, …).
            Err(WsError::Http(res)) => {
                let body = res.body().clone().unwrap_or_default();
                Err(ApiError::from_body(res.status().as_u16(), &body).into())
            }
            Err(e) => Err(e).context("opening the WebSocket failed"),
        }
    }

    /// Open and immediately close a tunnel, so problems (no deployment, no
    /// permission, nothing listening) surface before anything else starts.
    pub async fn check(&self) -> anyhow::Result<()> {
        let mut ws = self.open().await?;
        let _ = ws.close(None).await;
        Ok(())
    }
}

/// Bind the local listener: `127.0.0.1:<port>`, or a free port.
pub async fn listen(port: Option<u16>) -> anyhow::Result<TcpListener> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port.unwrap_or(0)));
    TcpListener::bind(addr).await.with_context(|| match port {
        Some(p) => format!("cannot listen on 127.0.0.1:{p} (in use?)"),
        None => "cannot open a local port".to_owned(),
    })
}

/// Accept local connections forever, each through its own tunnel.
pub async fn serve(listener: TcpListener, target: Target, verbose: bool) {
    loop {
        let Ok((conn, peer)) = listener.accept().await else {
            continue;
        };
        let target = target.clone();
        tokio::spawn(async move {
            let _ = conn.set_nodelay(true);
            match target.open().await {
                Ok(ws) => {
                    if verbose {
                        eprintln!("tunnel: {peer} connected");
                    }
                    pipe(conn, ws).await;
                    if verbose {
                        eprintln!("tunnel: {peer} disconnected");
                    }
                }
                Err(e) => eprintln!("tunnel: cannot connect {peer}: {e:#}"),
            }
        });
    }
}

/// Copy bytes both ways until either side closes.
async fn pipe(conn: TcpStream, ws: Ws) {
    let (mut read, mut write) = conn.into_split();
    let (mut sink, mut stream) = ws.split();
    let up = async {
        let mut buf = vec![0u8; CHUNK];
        loop {
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sink
                        .send(Message::Binary(buf[..n].to_vec().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            }
        }
        let _ = sink.close().await;
    };
    let down = async {
        while let Some(Ok(msg)) = stream.next().await {
            match msg {
                Message::Binary(b) if write.write_all(&b).await.is_err() => break,
                Message::Close(frame) => {
                    if let Some(f) = frame.filter(|f| !f.reason.is_empty()) {
                        eprintln!("tunnel closed: {}", f.reason);
                    }
                    break;
                }
                _ => {}
            }
        }
        let _ = write.shutdown().await;
    };
    tokio::pin!(up, down);
    tokio::select! {
        _ = &mut down => {}
        // The local side is done; let the server's close (and any last bytes) through.
        _ = &mut up => {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut down).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tunnel_urls_follow_the_instance_scheme() {
        let base = Url::parse("https://cloud.example.com/").unwrap();
        assert_eq!(
            tunnel_url(&base, "abc", None).unwrap().as_str(),
            "wss://cloud.example.com/api/service-instances/abc/tunnel"
        );
        let base = Url::parse("http://localhost:8080/").unwrap();
        assert_eq!(
            tunnel_url(&base, "abc", Some(5432)).unwrap().as_str(),
            "ws://localhost:8080/api/service-instances/abc/tunnel?port=5432"
        );
    }
}
