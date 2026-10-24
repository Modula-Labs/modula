//! The edge's HTTP access log: Caddy writes one JSON line per request of a
//! deployment's route to a loopback TCP listener of ours; each line becomes
//! an `HttpRequestLog` and goes to the server in batches (like `logship`).
//!
//! Reading never waits for the server: Caddy writes in the request's path,
//! so lines that don't fit the bounded buffer are counted and dropped.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use modula_proto::node::v1 as pb;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const EVERY: Duration = Duration::from_secs(2);
const BUFFER: usize = 50_000;
const PER_BATCH: usize = 5_000;
/// Longer lines are skipped (Caddy's are well under this).
const MAX_LINE: usize = 64 * 1024;
const MAX_PATH: usize = 2048;
const MAX_AGENT: usize = 512;

#[derive(Default)]
pub struct AccessLog {
    shared: Mutex<Shared>,
}

#[derive(Default)]
struct Shared {
    requests: Vec<pb::HttpRequestLog>,
    dropped: u32,
}

#[derive(Deserialize)]
struct Line {
    ts: f64,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    status: u32,
    request: Request,
    /// Added by the route's `log_append` handlers (see `caddy::render`).
    #[serde(default)]
    deployment_id: String,
    #[serde(default)]
    proxied: String,
    #[serde(default)]
    upstream: String,
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    remote_ip: String,
    #[serde(default)]
    client_ip: String,
    #[serde(default)]
    method: String,
    #[serde(default)]
    host: String,
    #[serde(default)]
    uri: String,
    #[serde(default)]
    headers: std::collections::HashMap<String, Vec<String>>,
}

fn cut(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
}

/// One access log line as a request; None for lines that aren't a
/// deployment's request (this node's own routes, the 404 fallback).
pub fn parse(line: &[u8]) -> Option<pb::HttpRequestLog> {
    let l: Line = serde_json::from_slice(line).ok()?;
    if l.deployment_id.is_empty() {
        return None;
    }
    let r = l.request;
    // Behind Cloudflare, Caddy's client_ip is CF-Connecting-IP (only for
    // connections from Cloudflare's ranges); otherwise the peer address.
    let client_ip = if l.proxied == "1" && !r.client_ip.is_empty() {
        r.client_ip
    } else {
        r.remote_ip
    };
    let host = r
        .host
        .rsplit_once(':')
        .filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit()))
        .map_or(r.host.as_str(), |(h, _)| h)
        .to_ascii_lowercase();
    let path = r.uri.split('?').next().unwrap_or_default().to_owned();
    let user_agent = r
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        .and_then(|(_, v)| v.first().cloned())
        .unwrap_or_default();
    Some(pb::HttpRequestLog {
        deployment_id: l.deployment_id,
        at_unix_ms: (l.ts * 1000.0) as i64,
        host,
        method: cut(r.method, 16),
        path: cut(path, MAX_PATH),
        status: l.status,
        duration_us: (l.duration.max(0.0) * 1e6).min(f64::from(u32::MAX)) as u32,
        bytes: l.size,
        client_ip: cut(client_ip, 64),
        upstream: cut(l.upstream, 64),
        user_agent: cut(user_agent, MAX_AGENT),
    })
}

impl AccessLog {
    fn push(&self, r: pb::HttpRequestLog) {
        let mut s = self.shared.lock().expect("access lock");
        if s.requests.len() >= BUFFER {
            s.dropped = s.dropped.saturating_add(1);
        } else {
            s.requests.push(r);
        }
    }

    /// Accept Caddy's connections (from loopback only) and read their lines.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, stop: CancellationToken) {
        loop {
            let (conn, peer) = tokio::select! {
                a = listener.accept() => match a { Ok(a) => a, Err(_) => continue },
                _ = stop.cancelled() => return,
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let me = self.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = me.read(conn) => {}
                    _ = stop.cancelled() => {}
                }
            });
        }
    }

    async fn read<R: tokio::io::AsyncRead + Unpin>(&self, mut conn: R) {
        let mut pending: Vec<u8> = Vec::new();
        let mut skipping = false;
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let n = match conn.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            let mut rest = &chunk[..n];
            while let Some(i) = rest.iter().position(|&b| b == b'\n') {
                if !skipping {
                    pending.extend_from_slice(&rest[..i]);
                    if let Some(r) = parse(&pending) {
                        self.push(r);
                    }
                }
                pending.clear();
                skipping = false;
                rest = &rest[i + 1..];
            }
            if !skipping {
                pending.extend_from_slice(rest);
                if pending.len() > MAX_LINE {
                    pending.clear();
                    skipping = true;
                }
            }
        }
    }

    /// Send what was collected every [`EVERY`] until `stop`.
    pub async fn ship(
        self: Arc<Self>,
        outbox: mpsc::Sender<pb::NodeMessage>,
        stop: CancellationToken,
    ) {
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tick.tick() => self.flush(&outbox),
            }
        }
    }

    fn flush(&self, outbox: &mpsc::Sender<pb::NodeMessage>) {
        loop {
            // Leave half the outbox to everything else.
            if outbox.capacity() * 2 < outbox.max_capacity() {
                return;
            }
            let batch = {
                let mut s = self.shared.lock().expect("access lock");
                if s.requests.is_empty() && s.dropped == 0 {
                    return;
                }
                let n = s.requests.len().min(PER_BATCH);
                pb::HttpLogBatch {
                    requests: s.requests.drain(..n).collect(),
                    dropped: std::mem::take(&mut s.dropped),
                }
            };
            let more = batch.requests.len() == PER_BATCH;
            let m = pb::NodeMessage {
                msg: Some(pb::node_message::Msg::HttpLogBatch(batch)),
            };
            if let Err(mpsc::error::TrySendError::Full(m) | mpsc::error::TrySendError::Closed(m)) =
                outbox.try_send(m)
            {
                if let Some(pb::node_message::Msg::HttpLogBatch(b)) = m.msg {
                    let mut s = self.shared.lock().expect("access lock");
                    let rest = std::mem::take(&mut s.requests);
                    s.requests = b.requests;
                    s.requests.extend(rest);
                    s.dropped = s.dropped.saturating_add(b.dropped);
                }
                return;
            }
            if !more {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line as Caddy 2.11 writes it for a request through a route with
    /// our `log_append` fields.
    const LINE: &str = r#"{"level":"info","ts":1792867377.5516741,"logger":"http.log.access.modula","msg":"handled request","request":{"remote_ip":"172.70.1.2","remote_port":"41342","client_ip":"198.51.100.7","proto":"HTTP/2.0","method":"GET","host":"API.example.com:443","uri":"/users/42?token=secret","headers":{"User-Agent":["curl/8.5.0"],"Accept":["*/*"],"Cf-Connecting-Ip":["198.51.100.7"]}},"bytes_read":0,"user_id":"","duration":0.012345,"size":1834,"status":200,"resp_headers":{"Content-Type":["application/json"]},"deployment_id":"dep1","proxied":"1","upstream":"10.88.0.2:8080"}"#;

    #[test]
    fn parses_a_caddy_line() {
        let r = parse(LINE.as_bytes()).unwrap();
        assert_eq!(r.deployment_id, "dep1");
        assert_eq!(r.at_unix_ms, 1_792_867_377_551);
        assert_eq!(r.host, "api.example.com");
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/users/42", "the query string is never kept");
        assert_eq!(r.status, 200);
        assert_eq!(r.duration_us, 12_345);
        assert_eq!(r.bytes, 1834);
        assert_eq!(r.client_ip, "198.51.100.7");
        assert_eq!(r.upstream, "10.88.0.2:8080");
        assert_eq!(r.user_agent, "curl/8.5.0");
    }

    #[test]
    fn the_cloudflare_client_ip_counts_only_on_proxied_domains() {
        let line = LINE.replace(r#""proxied":"1""#, r#""proxied":"0""#);
        assert_eq!(parse(line.as_bytes()).unwrap().client_ip, "172.70.1.2");
    }

    #[test]
    fn lines_without_a_deployment_are_skipped() {
        let line = LINE.replace(r#","deployment_id":"dep1""#, "");
        assert!(parse(line.as_bytes()).is_none());
        assert!(parse(b"not json").is_none());
        assert!(parse(b"{}").is_none());
    }

    #[tokio::test]
    async fn reads_lines_across_reads_and_skips_huge_ones() {
        let log = AccessLog::default();
        let huge = format!("{}\n", "x".repeat(MAX_LINE * 2));
        let input = format!("{LINE}\n{huge}{LINE}\n{LINE}");
        log.read(input.as_bytes()).await;
        // The last line has no newline yet: not a complete line.
        assert_eq!(log.shared.lock().unwrap().requests.len(), 2);
    }

    #[test]
    fn a_full_outbox_keeps_the_requests() {
        let log = AccessLog::default();
        log.push(parse(LINE.as_bytes()).unwrap());
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(pb::NodeMessage::default()).unwrap();
        log.flush(&tx);
        assert_eq!(log.shared.lock().unwrap().requests.len(), 1);
        rx.try_recv().unwrap();
        log.flush(&tx);
        match rx.try_recv().unwrap().msg {
            Some(pb::node_message::Msg::HttpLogBatch(b)) => assert_eq!(b.requests.len(), 1),
            other => panic!("unexpected {other:?}"),
        }
    }
}
