//! The wake proxy: where the edge sends requests for a sleeping (scaled to
//! zero) deployment.
//!
//! While a deployment sleeps, its Caddy route dials this loopback listener
//! and sets `X-Modula-Wake: <deployment> <port> <key>` on the request (see
//! `caddy::render`). The proxy reads the request head, asks the [`Waker`] to
//! wake the deployment, and once it serves, connects to `<its address>:<port>`,
//! replays the head (without that header) and splices both directions, so the
//! held request, its body and any upgrade (websockets) reach the app. A
//! deployment that is already awake is spliced at once (requests arriving
//! before the route points back at the container).
//!
//! What waking means is up to the registered [`Wakeable`] (a stopped
//! container: [`ContainerTarget`]). Every request arriving during one start
//! waits for that same start; the start runs on its own task, so a client
//! giving up doesn't cancel it. The key is random per process: only this
//! node's Caddy config carries it.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::runtime::Runtime;

pub const HEADER: &str = "X-Modula-Wake";
/// The longest a cold start may take before the held requests get a 504.
pub const MAX_WAKE: Duration = Duration::from_secs(60);
/// How often a starting container is checked for readiness.
const READY_POLL: Duration = Duration::from_millis(50);
const MAX_HEAD: usize = 64 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WakeError {
    #[error("nothing to wake")]
    Unknown,
    #[error("not ready within {0}s")]
    Timeout(u64),
    #[error("{0}")]
    Failed(String),
}

impl WakeError {
    /// The status the held request gets.
    pub fn status(&self) -> (u16, &'static str) {
        match self {
            Self::Unknown => (404, "Not Found"),
            Self::Timeout(_) => (504, "Gateway Timeout"),
            Self::Failed(_) => (502, "Bad Gateway"),
        }
    }
}

/// Something the proxy can wake: starts it and waits until it serves.
#[async_trait]
pub trait Wakeable: Send + Sync + 'static {
    /// Returns the address it serves on (the port comes from the route).
    async fn start(&self) -> Result<IpAddr, WakeError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Going to sleep (the route moves here, then the workload stops);
    /// a request waits for it to finish, then wakes it.
    Draining,
    Sleeping,
    Waking,
    Awake(IpAddr),
    /// The last start failed; the next request tries again.
    Failed(WakeError),
}

struct Entry {
    state: Arc<watch::Sender<State>>,
    target: Arc<dyn Wakeable>,
    /// Spliced connections open now.
    busy: u32,
    last_active: Instant,
    /// The last successful start's duration, until taken.
    cold_start: Option<Duration>,
}

pub struct Waker {
    addr: SocketAddr,
    key: String,
    entries: Mutex<HashMap<String, Entry>>,
    changed: Notify,
}

impl Waker {
    /// A waker whose proxy listens on `addr` (see [`Waker::serve`]).
    pub fn new(addr: SocketAddr) -> Arc<Self> {
        let mut b = [0u8; 16];
        let _ = getrandom::fill(&mut b);
        Arc::new(Self {
            addr,
            key: hex::encode(b),
            entries: Mutex::new(HashMap::new()),
            changed: Notify::new(),
        })
    }

    /// Where the edge sends a sleeping deployment's requests.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The [`HEADER`] value for requests to `deployment` on `port`.
    pub fn header(&self, deployment: &str, port: u16) -> String {
        format!("{deployment} {port} {}", self.key)
    }

    /// Notified after a start finished (well or not).
    pub async fn changed(&self) {
        self.changed.notified().await
    }

    /// `deployment` is going to sleep; `target` wakes it.
    pub fn draining(&self, deployment: &str, target: Arc<dyn Wakeable>) {
        let mut map = self.entries.lock().expect("waker lock");
        match map.get_mut(deployment) {
            Some(e) => {
                e.target = target;
                e.state.send_replace(State::Draining);
            }
            None => {
                map.insert(
                    deployment.to_owned(),
                    Entry {
                        state: Arc::new(watch::channel(State::Draining).0),
                        target,
                        busy: 0,
                        last_active: Instant::now(),
                        cold_start: None,
                    },
                );
            }
        }
    }

    /// `deployment` stopped: the next request wakes it.
    pub fn asleep(&self, deployment: &str) {
        if let Some(e) = self.entries.lock().expect("waker lock").get(deployment) {
            e.state.send_replace(State::Sleeping);
        }
    }

    pub fn state(&self, deployment: &str) -> Option<State> {
        self.entries
            .lock()
            .expect("waker lock")
            .get(deployment)
            .map(|e| e.state.borrow().clone())
    }

    /// The last cold start's duration, once.
    pub fn take_cold_start(&self, deployment: &str) -> Option<Duration> {
        self.entries
            .lock()
            .expect("waker lock")
            .get_mut(deployment)
            .and_then(|e| e.cold_start.take())
    }

    /// When a request last came through the proxy (now while one is open).
    pub fn last_active(&self, deployment: &str) -> Option<Instant> {
        let map = self.entries.lock().expect("waker lock");
        map.get(deployment).map(|e| {
            if e.busy > 0 {
                Instant::now()
            } else {
                e.last_active
            }
        })
    }

    /// Forget deployments `keep` says no to (no longer on this node).
    pub fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.entries
            .lock()
            .expect("waker lock")
            .retain(|d, _| keep(d));
    }

    /// Wake `deployment` (or join the start under way) and return where it serves.
    pub async fn wake(self: &Arc<Self>, deployment: &str) -> Result<IpAddr, WakeError> {
        // Whether this call has seen a start begin: a failure from before
        // that is retried, one after it is the answer.
        let mut started = false;
        loop {
            let mut rx = {
                let mut map = self.entries.lock().expect("waker lock");
                let e = map.get_mut(deployment).ok_or(WakeError::Unknown)?;
                e.last_active = Instant::now();
                // Subscribed first, so a change after the read is never missed.
                let mut rx = e.state.subscribe();
                let now = rx.borrow_and_update().clone();
                match now {
                    State::Awake(ip) => return Ok(ip),
                    State::Failed(err) if started => return Err(err),
                    State::Sleeping | State::Failed(_) => {
                        self.start(deployment, e);
                        started = true;
                    }
                    State::Waking => started = true,
                    State::Draining => {}
                }
                rx
            };
            if rx.changed().await.is_err() {
                return Err(WakeError::Unknown);
            }
        }
    }

    fn start(self: &Arc<Self>, deployment: &str, e: &mut Entry) {
        e.state.send_replace(State::Waking);
        let (state, target, me, dep) = (
            e.state.clone(),
            e.target.clone(),
            self.clone(),
            deployment.to_owned(),
        );
        tokio::spawn(async move {
            let began = Instant::now();
            let result = target.start().await;
            if let Some(e) = me.entries.lock().expect("waker lock").get_mut(&dep) {
                e.last_active = Instant::now();
                if result.is_ok() {
                    e.cold_start = Some(began.elapsed());
                }
            }
            match &result {
                Ok(_) => {
                    tracing::info!(deployment = %dep, ms = began.elapsed().as_millis() as u64, "woke up")
                }
                Err(e) => tracing::warn!(deployment = %dep, error = %e, "wake failed"),
            }
            state.send_replace(match result {
                Ok(ip) => State::Awake(ip),
                Err(e) => State::Failed(e),
            });
            me.changed.notify_one();
        });
    }

    fn hold(self: &Arc<Self>, deployment: &str) -> Busy {
        if let Some(e) = self.entries.lock().expect("waker lock").get_mut(deployment) {
            e.busy += 1;
        }
        Busy {
            waker: self.clone(),
            deployment: deployment.to_owned(),
        }
    }

    /// Serve the proxy until `stop`. Only loopback peers (Caddy) are served.
    pub async fn serve(self: Arc<Self>, listener: TcpListener, stop: CancellationToken) {
        loop {
            let (conn, peer) = tokio::select! {
                a = listener.accept() => match a { Ok(a) => a, Err(_) => continue },
                _ = stop.cancelled() => return,
            };
            if !peer.ip().is_loopback() {
                continue;
            }
            let (me, stop) = (self.clone(), stop.clone());
            tokio::spawn(async move {
                tokio::select! {
                    _ = me.handle(conn) => {}
                    _ = stop.cancelled() => {}
                }
            });
        }
    }

    async fn handle(self: Arc<Self>, mut conn: TcpStream) {
        let (mut buf, head_len) =
            match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut conn)).await {
                Ok(Ok(Some(head))) => head,
                _ => return respond(&mut conn, 400, "Bad Request", "").await,
            };
        let Some((line, deployment, port)) = find_wake(&buf[..head_len], &self.key) else {
            return respond(&mut conn, 403, "Forbidden", "").await;
        };
        buf.drain(line);
        let _busy = self.hold(&deployment);
        let ip = match self.wake(&deployment).await {
            Ok(ip) => ip,
            Err(e) => {
                let (status, reason) = e.status();
                let body = match e {
                    WakeError::Timeout(_) => "The service did not start in time.",
                    _ => "The service could not be started.",
                };
                return respond(&mut conn, status, reason, body).await;
            }
        };
        let addr = SocketAddr::new(ip, port);
        let mut upstream =
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
                Ok(Ok(s)) => s,
                _ => {
                    return respond(
                        &mut conn,
                        502,
                        "Bad Gateway",
                        "The service refused the connection.",
                    )
                    .await;
                }
            };
        if upstream.write_all(&buf).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut conn, &mut upstream).await;
    }
}

/// Keeps a deployment from sleeping while a spliced connection is open.
struct Busy {
    waker: Arc<Waker>,
    deployment: String,
}

impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(e) = self
            .waker
            .entries
            .lock()
            .expect("waker lock")
            .get_mut(&self.deployment)
        {
            e.busy = e.busy.saturating_sub(1);
            e.last_active = Instant::now();
        }
    }
}

/// Reads until the end of the request head. Returns everything read (the
/// head and whatever of the body came with it) and the head's length; None
/// when the peer closed first or the head is too long.
async fn read_head(conn: &mut TcpStream) -> std::io::Result<Option<(Vec<u8>, usize)>> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let n = conn.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            return Ok(Some((buf, from + i + 4)));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    }
}

/// The wake header's line (as a byte range, CRLF included), deployment and
/// port, when its key is `key`.
fn find_wake(head: &[u8], key: &str) -> Option<(std::ops::Range<usize>, String, u16)> {
    let mut at = 0;
    for line in head.split_inclusive(|&b| b == b'\n') {
        let range = at..at + line.len();
        at += line.len();
        let text = std::str::from_utf8(line).ok()?.trim_end();
        let Some((name, value)) = text.split_once(':') else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case(HEADER) {
            continue;
        }
        let mut parts = value.split_whitespace();
        let (Some(dep), Some(port), Some(k), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let port = port.parse::<u16>().ok().filter(|p| *p > 0)?;
        if k != key || dep.parse::<modula_id::Id>().is_err() {
            return None;
        }
        return Some((range, dep.to_owned(), port));
    }
    None
}

async fn respond(conn: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.write_all(msg.as_bytes()).await;
    let _ = conn.shutdown().await;
    // Read what's left of the request so closing doesn't reset the
    // connection before Caddy has the response.
    let mut sink = [0u8; 4096];
    let _ = tokio::time::timeout(Duration::from_secs(1), async {
        while matches!(conn.read(&mut sink).await, Ok(n) if n > 0) {}
    })
    .await;
}

/// A stopped container of a sleeping deployment: started, then ready once
/// its health check passes (or, without one, once its port accepts
/// connections), within `timeout`. A start that times out stops it again.
pub struct ContainerTarget {
    pub runtime: Arc<dyn Runtime>,
    pub container_id: String,
    pub environment_id: String,
    pub port: u16,
    /// Empty = no HTTP health check.
    pub health_path: String,
    pub timeout: Duration,
}

#[async_trait]
impl Wakeable for ContainerTarget {
    async fn start(&self) -> Result<IpAddr, WakeError> {
        let (id, env) = (self.container_id.as_str(), self.environment_id.as_str());
        self.runtime
            .start(id)
            .await
            .map_err(|e| WakeError::Failed(format!("cannot start the container: {e:#}")))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let containers = self
                .runtime
                .containers()
                .await
                .map_err(|e| WakeError::Failed(format!("{e:#}")))?;
            match containers.iter().find(|c| c.id == id) {
                Some(c) if c.running => {}
                Some(c) => {
                    return Err(WakeError::Failed(format!(
                        "the container exited while starting (code {})",
                        c.exit_code
                    )));
                }
                None => return Err(WakeError::Failed("the container is gone".into())),
            }
            let ready = if self.health_path.is_empty() {
                self.runtime.accepts(id, env, self.port).await
            } else {
                self.runtime
                    .probe(id, env, self.port, &self.health_path)
                    .await
                    .unwrap_or(false)
            };
            if ready && let Ok(Some(ip)) = self.runtime.address(id, env).await {
                return Ok(ip);
            }
            if Instant::now() >= deadline {
                if let Err(e) = self.runtime.stop(id).await {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        "cannot stop a container that didn't wake"
                    );
                }
                return Err(WakeError::Timeout(self.timeout.as_secs()));
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Wakes after `delay`, counting starts; fails when `fail` is set.
    struct Slow {
        delay: Duration,
        starts: AtomicU32,
        fail: Option<WakeError>,
    }

    #[async_trait]
    impl Wakeable for Slow {
        async fn start(&self) -> Result<IpAddr, WakeError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            match &self.fail {
                Some(e) => Err(e.clone()),
                None => Ok(IpAddr::from([127, 0, 0, 1])),
            }
        }
    }

    fn dep() -> String {
        modula_id::Id::new().to_string()
    }

    /// An "app" on loopback answering every request with its path.
    async fn app() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let Ok(Some((buf, n))) = read_head(&mut c).await else {
                        return;
                    };
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let leaked = head.to_ascii_lowercase().contains("x-modula-wake");
                    let body = format!("{path} leaked={leaked}");
                    let _ = c
                        .write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes())
                        .await;
                });
            }
        });
        port
    }

    async fn proxy() -> (Arc<Waker>, CancellationToken) {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let w = Waker::new(l.local_addr().unwrap());
        let stop = CancellationToken::new();
        tokio::spawn(w.clone().serve(l, stop.clone()));
        (w, stop)
    }

    /// A request as Caddy forwards it to the proxy.
    async fn get(w: &Waker, header: &str, path: &str) -> String {
        let mut c = TcpStream::connect(w.addr()).await.unwrap();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: api.example.com\r\n{HEADER}: {header}\r\nConnection: close\r\n\r\n"
        );
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn a_cold_start_serves_the_held_requests_with_one_start() {
        let (w, _stop) = proxy().await;
        let port = app().await;
        let d = dep();
        let target = Arc::new(Slow {
            delay: Duration::from_millis(300),
            starts: AtomicU32::new(0),
            fail: None,
        });
        w.draining(&d, target.clone());
        w.asleep(&d);
        let header = w.header(&d, port);
        let reqs: Vec<_> = (0..5)
            .map(|i| {
                let (w, header) = (w.clone(), header.clone());
                tokio::spawn(async move { get(&w, &header, &format!("/r{i}")).await })
            })
            .collect();
        for (i, r) in reqs.into_iter().enumerate() {
            let out = r.await.unwrap();
            assert!(out.starts_with("HTTP/1.1 200"), "{out}");
            assert!(out.ends_with(&format!("/r{i} leaked=false")), "{out}");
        }
        assert_eq!(target.starts.load(Ordering::SeqCst), 1, "one shared start");
        assert_eq!(
            w.state(&d),
            Some(State::Awake(IpAddr::from([127, 0, 0, 1])))
        );
        let cold = w.take_cold_start(&d).unwrap();
        assert!(cold >= Duration::from_millis(300), "{cold:?}");
        assert_eq!(w.take_cold_start(&d), None, "taken once");
        // Awake: spliced straight away.
        assert!(
            get(&w, &header, "/again")
                .await
                .ends_with("/again leaked=false")
        );
        assert_eq!(target.starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failures_answer_the_held_request_and_are_retried() {
        let (w, _stop) = proxy().await;
        let d = dep();
        let timeout = Arc::new(Slow {
            delay: Duration::ZERO,
            starts: AtomicU32::new(0),
            fail: Some(WakeError::Timeout(60)),
        });
        w.draining(&d, timeout.clone());
        w.asleep(&d);
        let out = get(&w, &w.header(&d, 1), "/").await;
        assert!(out.starts_with("HTTP/1.1 504"), "{out}");
        // The next request tries again.
        let out = get(&w, &w.header(&d, 1), "/").await;
        assert!(out.starts_with("HTTP/1.1 504"), "{out}");
        assert_eq!(timeout.starts.load(Ordering::SeqCst), 2);

        w.draining(
            &d,
            Arc::new(Slow {
                delay: Duration::ZERO,
                starts: AtomicU32::new(0),
                fail: Some(WakeError::Failed("exited".into())),
            }),
        );
        w.asleep(&d);
        assert!(
            get(&w, &w.header(&d, 1), "/")
                .await
                .starts_with("HTTP/1.1 502")
        );
    }

    #[tokio::test]
    async fn only_requests_with_this_nodes_key_are_served() {
        let (w, _stop) = proxy().await;
        let d = dep();
        let ok = w.header(&d, 8080);
        let wrong = ok.replace(&w.key, "0000");
        assert!(get(&w, &wrong, "/").await.starts_with("HTTP/1.1 403"));
        assert!(
            get(&w, &format!("{d} 8080"), "/")
                .await
                .starts_with("HTTP/1.1 403")
        );
        // The right key, but nothing registered under it.
        assert!(get(&w, &ok, "/").await.starts_with("HTTP/1.1 404"));
    }

    #[tokio::test]
    async fn a_request_while_draining_waits_then_wakes() {
        let w = Waker::new("127.0.0.1:1".parse().unwrap());
        let d = dep();
        let target = Arc::new(Slow {
            delay: Duration::ZERO,
            starts: AtomicU32::new(0),
            fail: None,
        });
        w.draining(&d, target.clone());
        let waiting = tokio::spawn({
            let (w, d) = (w.clone(), d.clone());
            async move { w.wake(&d).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            target.starts.load(Ordering::SeqCst),
            0,
            "not while draining"
        );
        w.asleep(&d);
        assert!(waiting.await.unwrap().is_ok());
        assert_eq!(target.starts.load(Ordering::SeqCst), 1);
    }

    /// A real nginx container put to sleep with the runtime's `stop` and
    /// woken by a request through the proxy; prints the cold start. Needs
    /// Docker and nginx:alpine; `cargo test -p modula-node -- --ignored
    /// --nocapture real_container`. The app is reached on a published
    /// loopback port (container addresses aren't reachable from a Docker
    /// Desktop host), so readiness is checked there.
    #[tokio::test]
    #[ignore]
    async fn a_real_container_cold_starts() {
        use std::process::Command;
        struct Published {
            rt: Arc<dyn Runtime>,
            id: String,
            port: u16,
        }
        #[async_trait]
        impl Wakeable for Published {
            async fn start(&self) -> Result<IpAddr, WakeError> {
                self.rt
                    .start(&self.id)
                    .await
                    .map_err(|e| WakeError::Failed(e.to_string()))?;
                let deadline = Instant::now() + MAX_WAKE;
                while Instant::now() < deadline {
                    if let Ok(mut c) = TcpStream::connect(("127.0.0.1", self.port)).await {
                        let _ = c
                            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                            .await;
                        let mut head = [0u8; 12];
                        if c.read_exact(&mut head).await.is_ok()
                            && head.starts_with(b"HTTP/1.1 200")
                        {
                            return Ok(IpAddr::from([127, 0, 0, 1]));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(WakeError::Timeout(MAX_WAKE.as_secs()))
            }
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let name = format!("modula-wake-test-{}", modula_id::Id::new());
        let out = Command::new("docker")
            .args(["run", "-d", "--name", &name, "-p"])
            .arg(format!("127.0.0.1:{port}:80"))
            .arg("nginx:alpine")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
        let rt: Arc<dyn Runtime> =
            Arc::new(crate::runtime::docker::DockerRuntime::connect().unwrap());
        let outcome = async {
            let target = Arc::new(Published {
                rt: rt.clone(),
                id: id.clone(),
                port,
            });
            target.start().await.expect("first start");
            rt.stop(&id).await.unwrap();
            let running = Command::new("docker")
                .args(["inspect", "-f", "{{.State.Running}}", &id])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&running.stdout).trim(),
                "false",
                "stopped, kept"
            );

            let (w, _stop) = proxy().await;
            let d = dep();
            w.draining(&d, target);
            w.asleep(&d);
            let began = std::time::Instant::now();
            let out = get(&w, &w.header(&d, port), "/").await;
            let total = began.elapsed();
            assert!(out.starts_with("HTTP/1.1 200"), "{out}");
            let cold = w.take_cold_start(&d).unwrap();
            println!(
                "cold start: {} ms (container start to first 200), request: {} ms",
                cold.as_millis(),
                total.as_millis()
            );
            // Awake: the next request doesn't start anything.
            let began = std::time::Instant::now();
            assert!(
                get(&w, &w.header(&d, port), "/")
                    .await
                    .starts_with("HTTP/1.1 200")
            );
            println!("warm request: {} ms", began.elapsed().as_millis());
        };
        let result = tokio::time::timeout(Duration::from_secs(120), outcome).await;
        let _ = Command::new("docker").args(["rm", "-f", &name]).output();
        result.expect("timed out");
    }

    #[test]
    fn the_wake_header_is_found_and_cut() {
        let key = "k1";
        let d = modula_id::Id::new().to_string();
        let head = format!(
            "GET / HTTP/1.1\r\nHost: a\r\nx-modula-wake: {d} 3000 {key}\r\nAccept: */*\r\n\r\n"
        );
        let (range, dep, port) = find_wake(head.as_bytes(), key).unwrap();
        assert_eq!((dep.as_str(), port), (d.as_str(), 3000));
        let mut cut = head.clone().into_bytes();
        cut.drain(range);
        assert_eq!(
            String::from_utf8(cut).unwrap(),
            "GET / HTTP/1.1\r\nHost: a\r\nAccept: */*\r\n\r\n"
        );
        for bad in [
            format!("{d} 3000"),
            format!("{d} 0 {key}"),
            format!("nope 3000 {key}"),
            format!("{d} 3000 {key} x"),
        ] {
            let head = format!("GET / HTTP/1.1\r\n{HEADER}: {bad}\r\n\r\n");
            assert!(find_wake(head.as_bytes(), key).is_none(), "{bad}");
        }
    }
}
