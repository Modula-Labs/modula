//! Caddy's on-demand TLS `ask` endpoint: before Caddy requests a certificate
//! for a hostname, it asks us. We answer 200 only for hostnames we route.
//!
//! Listens on loopback only and checks the Host header, so a DNS-rebinding
//! page in a browser on this machine can't reach it.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::normalize_host;

pub type Allowlist = Arc<RwLock<HashSet<String>>>;

/// The decision for one request line + Host header.
pub fn decide(request: &str, allowed: &HashSet<String>, own_host: &str) -> u16 {
    let mut lines = request.lines();
    let Some(line) = lines.next() else { return 400 };
    let host_ok = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("host"))
        .is_some_and(|(_, v)| v.trim() == own_host);
    if !host_ok {
        return 403;
    }
    let mut parts = line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return 400;
    };
    let Some(query) = target.strip_prefix("/ask?") else {
        return 404;
    };
    let domain = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("domain="))
        .and_then(normalize_host);
    match domain {
        Some(d) if allowed.contains(&d) => 200,
        _ => 403,
    }
}

pub async fn serve(listener: TcpListener, allowed: Allowlist, stop: CancellationToken) {
    let own_host = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    loop {
        let (mut conn, _) = tokio::select! {
            a = listener.accept() => match a { Ok(a) => a, Err(_) => continue },
            _ = stop.cancelled() => return,
        };
        let (allowed, own_host) = (allowed.clone(), own_host.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut buf)).await
            else {
                return;
            };
            let req = String::from_utf8_lossy(&buf[..n]);
            let status = {
                let set = allowed.read().expect("allowlist lock");
                decide(&req, &set, &own_host)
            };
            let reason = if status == 200 { "OK" } else { "Forbidden" };
            let _ = conn
                .write_all(format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: &str, host: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n")
    }

    #[test]
    fn answers_only_for_routed_hosts() {
        let allowed: HashSet<String> = ["api.apps.example.com".to_owned()].into();
        let me = "127.0.0.1:7102";
        assert_eq!(
            decide(&req("/ask?domain=api.apps.example.com", me), &allowed, me),
            200
        );
        assert_eq!(
            decide(&req("/ask?domain=API.apps.example.com.", me), &allowed, me),
            200
        );
        assert_eq!(
            decide(&req("/ask?domain=evil.example.com", me), &allowed, me),
            403
        );
        assert_eq!(decide(&req("/ask?domain=127.0.0.1", me), &allowed, me), 403);
        // DNS rebinding: right path, wrong Host.
        assert_eq!(
            decide(
                &req("/ask?domain=api.apps.example.com", "attacker.example:7102"),
                &allowed,
                me
            ),
            403
        );
        assert_eq!(decide(&req("/other", me), &allowed, me), 404);
    }
}
