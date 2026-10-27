//! TCP proxies: a public port on this node forwarded to one service's
//! container (databases reached from outside). Each port has a listener task;
//! its upstream is swapped in place when a new deployment takes over, so
//! clients reconnect to the same address after a redeploy.
//!
//! With an active ufw, each listening port gets its own `allow` rule
//! (comment `modula`), deleted when the listener stops; the first apply
//! also deletes tagged rules in the proxy range left by an earlier run.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

struct Proxy {
    upstream: Arc<RwLock<Option<SocketAddr>>>,
    task: JoinHandle<()>,
}

/// Public ports the server allocates for TCP proxies.
pub const PORT_RANGE: RangeInclusive<u16> = 20000..=29999;

#[derive(Default)]
pub struct TcpProxies {
    /// public port → its listener.
    proxies: Mutex<HashMap<u16, Proxy>>,
    /// Stale ufw rules were looked for (first apply).
    swept: AtomicBool,
}

impl TcpProxies {
    /// Make the running listeners match `wanted` (public port → upstream, or
    /// None while the service has no running container: connections are
    /// refused until it does).
    pub async fn apply(&self, wanted: &HashMap<u16, Option<SocketAddr>>) {
        let (mut to_bind, mut closed, mut opened) = (Vec::new(), Vec::new(), Vec::new());
        {
            let mut proxies = self.proxies.lock().expect("tcp proxies lock");
            proxies.retain(|port, p| {
                let keep = wanted.contains_key(port) && !p.task.is_finished();
                if !keep {
                    p.task.abort();
                    closed.push(*port);
                }
                keep
            });
            for (port, upstream) in wanted {
                match proxies.get(port) {
                    Some(p) => *p.upstream.write().expect("upstream lock") = *upstream,
                    None => to_bind.push((*port, *upstream)),
                }
            }
        }
        for (port, upstream) in to_bind {
            match TcpListener::bind(("0.0.0.0", port)).await {
                Ok(listener) => {
                    let upstream = Arc::new(RwLock::new(upstream));
                    let task = tokio::spawn(serve(listener, upstream.clone()));
                    tracing::info!(port, "tcp proxy listening");
                    self.proxies
                        .lock()
                        .expect("tcp proxies lock")
                        .insert(port, Proxy { upstream, task });
                    opened.push(port);
                }
                // Retried on the next apply (e.g. the port is briefly busy).
                Err(e) => tracing::warn!(port, error = %e, "cannot listen for a tcp proxy"),
            }
        }
        let sweep = !self.swept.swap(true, Ordering::Relaxed);
        if (sweep || !opened.is_empty() || !closed.is_empty()) && crate::access::ufw_active().await
        {
            if sweep {
                let listening = self.ports();
                closed.extend(
                    crate::access::ufw_open_ports(PORT_RANGE)
                        .await
                        .into_iter()
                        .filter(|p| !listening.contains(p)),
                );
                // Listeners from before this process may lack their rule.
                opened = listening;
            }
            update_ufw(&opened, &closed).await;
        }
    }

    pub fn ports(&self) -> Vec<u16> {
        let mut p: Vec<u16> = self
            .proxies
            .lock()
            .expect("tcp proxies lock")
            .keys()
            .copied()
            .collect();
        p.sort_unstable();
        p
    }
}

impl Drop for TcpProxies {
    fn drop(&mut self) {
        for p in self.proxies.lock().expect("tcp proxies lock").values() {
            p.task.abort();
        }
    }
}

/// Allow `opened` and delete `closed` in ufw (proxy ports only); failures warn.
async fn update_ufw(opened: &[u16], closed: &[u16]) {
    for &port in closed.iter().filter(|p| PORT_RANGE.contains(p)) {
        match crate::access::ufw_close(port).await {
            Ok(()) => tracing::info!(port, "closed a tcp proxy port in ufw"),
            Err(e) => tracing::warn!(
                port,
                error = format!("{e:#}"),
                "cannot close a tcp proxy port in ufw"
            ),
        }
    }
    for &port in opened.iter().filter(|p| PORT_RANGE.contains(p)) {
        if let Err(e) = crate::access::ufw_open(port).await {
            tracing::warn!(
                port,
                error = format!("{e:#}"),
                "cannot open a tcp proxy port in ufw"
            );
        }
    }
}

async fn serve(listener: TcpListener, upstream: Arc<RwLock<Option<SocketAddr>>>) {
    loop {
        let Ok((mut client, peer)) = listener.accept().await else {
            continue;
        };
        let target = *upstream.read().expect("upstream lock");
        tokio::spawn(async move {
            let Some(target) = target else {
                return; // dropping the client closes it
            };
            match TcpStream::connect(target).await {
                Ok(mut server) => {
                    let _ = client.set_nodelay(true);
                    let _ = server.set_nodelay(true);
                    let _ = copy_bidirectional(&mut client, &mut server).await;
                }
                Err(e) => {
                    tracing::debug!(%peer, %target, error = %e, "tcp proxy upstream unreachable")
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn echo_server() -> SocketAddr {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buf = [0u8; 64];
                    let n = s.read(&mut buf).await.unwrap();
                    s.write_all(&buf[..n]).await.unwrap();
                });
            }
        });
        addr
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn roundtrip(port: u16) -> Option<Vec<u8>> {
        let mut c = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
        c.write_all(b"ping").await.ok()?;
        let mut buf = Vec::new();
        c.read_to_end(&mut buf).await.ok()?;
        (!buf.is_empty()).then_some(buf)
    }

    #[tokio::test]
    async fn forwards_swaps_and_stops() {
        let (a, port) = (echo_server().await, free_port());
        let proxies = TcpProxies::default();
        proxies.apply(&HashMap::from([(port, Some(a))])).await;
        assert_eq!(roundtrip(port).await.as_deref(), Some(&b"ping"[..]));

        // No running container: accepted then closed.
        proxies.apply(&HashMap::from([(port, None)])).await;
        assert_eq!(roundtrip(port).await, None);

        let b = echo_server().await;
        proxies.apply(&HashMap::from([(port, Some(b))])).await;
        assert_eq!(roundtrip(port).await.as_deref(), Some(&b"ping"[..]));

        proxies.apply(&HashMap::new()).await;
        assert!(proxies.ports().is_empty());
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }
}
