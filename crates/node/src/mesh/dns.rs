//! The node's resolver for its containers, on its mesh address (`.1:53`).
//!
//! Containers keep Docker's embedded DNS, which answers names of services
//! running on this server itself (aliases on the environment's bridge) and
//! sends every other query here. This resolver then:
//!   * finds the asking container's environment by its source address (its
//!     mesh address: the query comes over the mesh network) and answers
//!     `<service>.modula.internal` (and `<service>`) with the healthy
//!     replicas of that service on other servers, shuffled, with a short TTL;
//!   * forwards anything else to the host's own resolvers;
//!   * refuses everybody else (peers' latency probes get a quick REFUSED).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, Record, RecordType, rdata::A};
use modula_proto::node::v1 as pb;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use super::Subnet;

pub const TTL: u32 = 5;
const SUFFIX: &str = ".modula.internal";
const FORWARD_TIMEOUT: Duration = Duration::from_secs(2);
const BIND_RETRY: Duration = Duration::from_secs(2);
const MAX_TCP: usize = 64;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BookData {
    /// This node's containers (and one-off containers): address → environment.
    pub local: HashMap<Ipv4Addr, String>,
    /// (environment, service) → healthy addresses on other nodes.
    pub names: HashMap<(String, String), Vec<Ipv4Addr>>,
    /// Peer nodes' host addresses.
    pub peers: HashSet<Ipv4Addr>,
}

impl BookData {
    pub fn from_spec(
        spec: &pb::Mesh,
        subnet: Subnet,
        leases: &BTreeMap<Ipv4Addr, String>,
        peers: Vec<Ipv4Addr>,
    ) -> Self {
        let mut b = Self {
            local: leases.iter().map(|(ip, env)| (*ip, env.clone())).collect(),
            peers: peers.into_iter().collect(),
            ..Self::default()
        };
        for env in &spec.environments {
            for m in &env.members {
                let Ok(ip) = m.ip.parse::<Ipv4Addr>() else {
                    continue;
                };
                if subnet.contains(ip) {
                    // Local replicas are Docker's to answer (same-server
                    // traffic never crosses the mesh network).
                    b.local.insert(ip, env.environment_id.clone());
                } else if m.healthy {
                    b.names
                        .entry((env.environment_id.clone(), m.service_name.clone()))
                        .or_default()
                        .push(ip);
                }
            }
        }
        b
    }
}

/// What to do with a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// NOERROR with these A records (none: the name exists, other type).
    Records(Vec<Ipv4Addr>),
    NxDomain,
    Refused,
    Forward,
}

#[derive(Default)]
pub struct Book {
    data: RwLock<BookData>,
}

impl Book {
    pub fn update(&self, data: BookData) {
        *self.data.write().expect("book lock") = data;
    }

    pub fn add_local(&self, ip: Ipv4Addr, environment: &str) {
        self.data
            .write()
            .expect("book lock")
            .local
            .insert(ip, environment.to_owned());
    }

    /// The answer for `name` (any case, trailing dot or not) asked by `src`.
    pub fn answer(&self, src: Ipv4Addr, name: &str, qtype: RecordType) -> Answer {
        let data = self.data.read().expect("book lock");
        let Some(env) = data.local.get(&src) else {
            return Answer::Refused;
        };
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let (service, internal) = match name.strip_suffix(SUFFIX) {
            Some(s) => (s, true),
            None => (name.as_str(), false),
        };
        if internal && (service.is_empty() || service.contains('.')) {
            return Answer::NxDomain;
        }
        if !internal && service.contains('.') {
            return Answer::Forward;
        }
        match data.names.get(&(env.clone(), service.to_owned())) {
            Some(ips) if !ips.is_empty() => {
                if qtype != RecordType::A {
                    return Answer::Records(Vec::new());
                }
                let mut ips = ips.clone();
                shuffle(&mut ips);
                Answer::Records(ips)
            }
            _ if internal => Answer::NxDomain,
            _ => Answer::Forward,
        }
    }
}

fn shuffle(ips: &mut [Ipv4Addr]) {
    for i in (1..ips.len()).rev() {
        let mut b = [0u8; 4];
        let _ = getrandom::fill(&mut b);
        let j = (u32::from_le_bytes(b) as usize) % (i + 1);
        ips.swap(i, j);
    }
}

/// The host's resolvers (`/etc/resolv.conf`; systemd-resolved's real ones
/// when it only lists its stub).
pub fn host_upstreams() -> Vec<SocketAddr> {
    let read = |path: &str| -> Vec<IpAddr> {
        std::fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().strip_prefix("nameserver"))
            .filter_map(|v| v.trim().parse().ok())
            .collect()
    };
    let mut ips = read("/etc/resolv.conf");
    if ips.iter().all(IpAddr::is_loopback) {
        let real = read("/run/systemd/resolve/resolv.conf");
        if !real.is_empty() {
            ips = real;
        }
    }
    ips.into_iter()
        .filter(|ip| !matches!(ip, IpAddr::V4(v4) if v4.octets()[..2] == [10, 80]))
        .map(|ip| SocketAddr::new(ip, 53))
        .collect()
}

/// The response bytes for one query packet from `src`.
async fn handle(book: &Book, src: IpAddr, packet: &[u8]) -> Option<Vec<u8>> {
    let req = Message::from_vec(packet).ok()?;
    if req.metadata.message_type != MessageType::Query {
        return None;
    }
    let reply = |code: ResponseCode, answers: Vec<Record>| {
        let mut m = Message::new(req.metadata.id, MessageType::Response, req.metadata.op_code);
        m.metadata.recursion_desired = req.metadata.recursion_desired;
        m.metadata.recursion_available = true;
        m.metadata.authoritative = code != ResponseCode::Refused;
        m.metadata.response_code = code;
        m.add_queries(req.queries.clone());
        m.add_answers(answers);
        m.to_vec().ok()
    };
    let IpAddr::V4(src) = src else {
        return reply(ResponseCode::Refused, Vec::new());
    };
    let query: &Query = match req.queries.as_slice() {
        [q] if req.metadata.op_code == OpCode::Query => q,
        _ => return reply(ResponseCode::Refused, Vec::new()),
    };
    match book.answer(src, &query.name().to_ascii(), query.query_type()) {
        Answer::Records(ips) => {
            let records = ips
                .into_iter()
                .map(|ip| Record::from_rdata(query.name().clone(), TTL, RData::A(A(ip))))
                .collect();
            reply(ResponseCode::NoError, records)
        }
        Answer::NxDomain => reply(ResponseCode::NXDomain, Vec::new()),
        Answer::Refused => reply(ResponseCode::Refused, Vec::new()),
        Answer::Forward => match forward(packet).await {
            Some(r) => Some(r),
            None => reply(ResponseCode::ServFail, Vec::new()),
        },
    }
}

/// Relays a query to the host's resolvers, first answer wins.
async fn forward(packet: &[u8]) -> Option<Vec<u8>> {
    for upstream in host_upstreams() {
        let bind: SocketAddr = match upstream {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (std::net::Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let Ok(sock) = UdpSocket::bind(bind).await else {
            continue;
        };
        if sock.send_to(packet, upstream).await.is_err() {
            continue;
        }
        let mut buf = vec![0u8; 65_535];
        let deadline = Instant::now() + FORWARD_TIMEOUT;
        while let Ok(Ok((n, from))) =
            tokio::time::timeout_at(deadline.into(), sock.recv_from(&mut buf)).await
        {
            // Only the upstream's answer to this query id.
            if from == upstream && n >= 2 && buf[..2] == packet[..2] {
                buf.truncate(n);
                return Some(buf);
            }
        }
    }
    None
}

/// Serve UDP and TCP on `addr:53` until the process stops. Binding is
/// retried: the address appears with the mesh network.
pub async fn serve(book: Arc<Book>, addr: Ipv4Addr) -> anyhow::Result<()> {
    let at = SocketAddr::from((addr, 53));
    let (udp, tcp) = loop {
        match (UdpSocket::bind(at).await, TcpListener::bind(at).await) {
            (Ok(u), Ok(t)) => break (Arc::new(u), t),
            (u, t) => {
                let e = u
                    .err()
                    .or(t.err())
                    .map(|e| e.to_string())
                    .unwrap_or_default();
                tracing::debug!(%at, error = e, "mesh resolver can't bind yet");
                tokio::time::sleep(BIND_RETRY).await;
            }
        }
    };
    tracing::info!(%at, "mesh resolver listening");
    let tcp_task = {
        let book = book.clone();
        tokio::spawn(async move {
            let slots = Arc::new(tokio::sync::Semaphore::new(MAX_TCP));
            loop {
                let Ok((mut conn, peer)) = tcp.accept().await else {
                    continue;
                };
                let Ok(permit) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let book = book.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let one = async {
                        let mut len = [0u8; 2];
                        conn.read_exact(&mut len).await?;
                        let mut q = vec![0u8; usize::from(u16::from_be_bytes(len))];
                        conn.read_exact(&mut q).await?;
                        if let Some(r) = handle(&book, peer.ip(), &q).await
                            && let Ok(n) = u16::try_from(r.len())
                        {
                            conn.write_all(&n.to_be_bytes()).await?;
                            conn.write_all(&r).await?;
                        }
                        Ok::<_, std::io::Error>(())
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(5), one).await;
                });
            }
        })
    };
    let _abort = AbortOnDrop(tcp_task);
    let mut buf = vec![0u8; 4096];
    loop {
        let (n, peer) = match udp.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let (udp, book, packet) = (udp.clone(), book.clone(), buf[..n].to_vec());
        tokio::spawn(async move {
            if let Some(r) = handle(&book, peer.ip(), &packet).await {
                let _ = udp.send_to(&r, peer).await;
            }
        });
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Round trip to a peer's resolver over the tunnel (None: no answer).
pub async fn probe(peer: Ipv4Addr, timeout: Duration) -> Option<Duration> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await.ok()?;
    let mut q = Message::query();
    q.metadata.id = 0x6d6e;
    q.add_query(Query::query(
        Name::from_ascii("probe.modula.internal.").ok()?,
        RecordType::A,
    ));
    let packet = q.to_vec().ok()?;
    let started = Instant::now();
    sock.send_to(&packet, (peer, 53)).await.ok()?;
    let mut buf = [0u8; 512];
    match tokio::time::timeout(timeout, sock.recv_from(&mut buf)).await {
        Ok(Ok((n, _))) if n >= 2 && buf[..2] == packet[..2] => Some(started.elapsed()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    fn book() -> Book {
        let member = |ip: &str, svc: &str, healthy| pb::MeshMember {
            ip: ip.into(),
            service_name: svc.into(),
            healthy,
            ..Default::default()
        };
        let spec = pb::Mesh {
            environments: vec![
                pb::MeshEnvironment {
                    environment_id: "prod".into(),
                    members: vec![
                        member("10.80.1.2", "api", true),
                        member("10.80.2.2", "db", true),
                        member("10.80.3.2", "db", true),
                        member("10.80.2.3", "cache", false),
                        // A local replica of db: Docker's to answer.
                        member("10.80.1.3", "db", true),
                    ],
                },
                pb::MeshEnvironment {
                    environment_id: "staging".into(),
                    members: vec![
                        member("10.80.1.4", "api", true),
                        member("10.80.2.9", "db", true),
                    ],
                },
            ],
            ..Default::default()
        };
        let b = Book::default();
        b.update(BookData::from_spec(
            &spec,
            Subnet { index: 1 },
            &BTreeMap::new(),
            vec![ip("10.80.2.1")],
        ));
        b
    }

    fn sorted(a: Answer) -> Answer {
        match a {
            Answer::Records(mut ips) => {
                ips.sort();
                Answer::Records(ips)
            }
            other => other,
        }
    }

    #[test]
    fn answers_healthy_remote_replicas_of_the_asking_environment() {
        let b = book();
        let prod = ip("10.80.1.2");
        assert_eq!(
            sorted(b.answer(prod, "DB.modula.internal.", RecordType::A)),
            Answer::Records(vec![ip("10.80.2.2"), ip("10.80.3.2")])
        );
        assert_eq!(
            sorted(b.answer(prod, "db", RecordType::A)),
            Answer::Records(vec![ip("10.80.2.2"), ip("10.80.3.2")])
        );
        // Other environments' services don't exist for it.
        assert_eq!(
            b.answer(ip("10.80.1.4"), "db.modula.internal", RecordType::A),
            Answer::Records(vec![ip("10.80.2.9")])
        );
        assert_eq!(
            b.answer(prod, "cache.modula.internal", RecordType::A),
            Answer::NxDomain
        );
        assert_eq!(
            b.answer(prod, "x.db.modula.internal", RecordType::A),
            Answer::NxDomain
        );
        assert_eq!(
            b.answer(prod, "db.modula.internal", RecordType::AAAA),
            Answer::Records(vec![])
        );
        assert_eq!(
            b.answer(prod, "example.com", RecordType::A),
            Answer::Forward
        );
        assert_eq!(b.answer(prod, "cache", RecordType::A), Answer::Forward);
        // Strangers, other nodes and their containers.
        for src in ["10.80.2.2", "10.80.2.1", "172.17.0.2"] {
            assert_eq!(
                b.answer(ip(src), "db", RecordType::A),
                Answer::Refused,
                "{src}"
            );
        }
    }

    #[tokio::test]
    async fn speaks_dns() {
        let b = book();
        let mut q = Message::query();
        q.metadata.id = 7;
        q.metadata.recursion_desired = true;
        q.add_query(Query::query(
            Name::from_ascii("db.modula.internal.").unwrap(),
            RecordType::A,
        ));
        let r = handle(&b, ip("10.80.1.2").into(), &q.to_vec().unwrap())
            .await
            .unwrap();
        let r = Message::from_vec(&r).unwrap();
        assert_eq!(r.metadata.id, 7);
        assert_eq!(r.metadata.response_code, ResponseCode::NoError);
        assert_eq!(r.answers.len(), 2);
        assert!(r.answers.iter().all(|a| a.ttl == TTL));
        let refused = handle(&b, ip("10.80.9.9").into(), &q.to_vec().unwrap())
            .await
            .unwrap();
        assert_eq!(
            Message::from_vec(&refused).unwrap().metadata.response_code,
            ResponseCode::Refused
        );
    }
}
