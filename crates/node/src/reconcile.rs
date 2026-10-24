//! Converges the runtime to the newest desired state.
//!
//! Each new workload gets a rollout task: [build →] pull → create → start →
//! health check → ACTIVE (or FAILED, after which its container is removed).
//! A build with an output directory (a static site hosted elsewhere) ends at
//! BUILT instead: the output is uploaded to the control plane and nothing
//! runs here. A cron service's rollout ends ACTIVE with nothing started: its
//! runs come one at a time ([`crate::cron`]). A function's rollout is
//! [build →] check → join the node's workerd → ACTIVE ([`crate::functions`]).
//! A periodic pass repairs drift and reports crashes and recoveries.
//! Containers and functions whose deployment is no longer desired are
//! stopped and removed; nothing without our labels is ever touched.
//!
//! Serverless deployments (`Workload.serverless`) scale to zero: once one
//! had no request for its idle time (access log, requests in flight at the
//! edge, connections through the wake proxy), its route moves to the wake
//! proxy ([`crate::edge::wake`]) and its container is stopped, not removed
//! (SLEEPING). The next request starts it again; it is ACTIVE once the wake
//! finished, with the cold start's duration. A stopped container of a
//! sleeping deployment is expected, never a crash. Without an edge and a wake
//! proxy serverless deployments just keep running.


use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use modula_id::Id;
use modula_proto::node::v1 as pb;
use pb::DeploymentPhase as Phase;
use pb::node_message::Msg;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::build::{BuildLog, BuildSupport, Built, LOCAL_PREFIX};
use crate::edge::tcp::TcpProxies;
use crate::edge::wake::{ContainerTarget, State as WakeState, Waker};
use crate::edge::{EdgeConfig, EdgeSink, ResolvedRoute, Site};
use crate::runtime::{Container, Runtime};

const REPAIR_EVERY: Duration = Duration::from_secs(2);
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// Without an HTTP health check, a container must stay up this long.
const SETTLE: Duration = Duration::from_secs(2);
/// A pre-deploy command (migrations) must finish within this.
const PRE_DEPLOY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How much of a failed pre-deploy command's output its error shows.
const PRE_DEPLOY_TAIL: usize = 1500;

struct Tracked {
    phase: Phase,
    message: String,
    rollout: Option<JoinHandle<()>>,
    /// The `restart_generation` last applied; `None` until first observed.
    restarted: Option<u32>,
    /// When it last became ACTIVE: a serverless deployment is idle from then
    /// at the earliest.
    active_since: tokio::time::Instant,
}

impl Tracked {
    fn new() -> Self {
        Self {
            phase: Phase::PhaseUnspecified,
            message: String::new(),
            rollout: None,
            restarted: None,
            active_since: tokio::time::Instant::now(),
        }
    }
}

/// Optional parts of a node: an edge (Caddy), a builder and database access.
#[derive(Clone, Default)]
pub struct Capabilities {
    pub edge: Option<Arc<dyn EdgeSink>>,
    pub build: Option<BuildSupport>,
    /// The Database tab ([`crate::db`]).
    pub db: Option<Arc<dyn crate::db::Executor>>,
    /// Upgrades on the control plane's request ([`crate::upgrade`]).
    pub upgrade: Option<Arc<dyn crate::upgrade::Upgrader>>,
    /// Private networking between servers ([`crate::mesh`]).
    pub mesh: Option<Arc<crate::mesh::Mesh>>,
    /// The edge's HTTP access log collector ([`crate::edge::access`]).
    pub access_log: Option<Arc<crate::edge::access::AccessLog>>,
    /// The wake proxy of sleeping serverless deployments.
    pub wake: Option<Arc<Waker>>,
    /// JavaScript functions in workerd ([`crate::functions`]).
    pub functions: Option<Arc<crate::functions::Functions>>,
}

pub struct Reconciler {
    runtime: Arc<dyn Runtime>,
    edge: Option<Arc<dyn EdgeSink>>,
    build: Option<BuildSupport>,
    mesh: Option<Arc<crate::mesh::Mesh>>,
    access_log: Option<Arc<crate::edge::access::AccessLog>>,
    /// Set only with an edge: nothing could wake a deployment otherwise.
    wake: Option<Arc<Waker>>,
    functions: Option<Arc<crate::functions::Functions>>,
    out: mpsc::Sender<pb::NodeMessage>,
    tracked: Mutex<HashMap<String, Tracked>>,
    applied: AtomicU64,
    tcp: TcpProxies,
}

/// Volumes this node manages (see the server's `volume_name`).
fn is_volume_name(name: &str) -> bool {
    name.strip_prefix("modula-vol-")
        .is_some_and(|id| id.parse::<modula_id::Id>().is_ok())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The server validates these too; the node does not trust them blindly
/// because they end up in container names and labels.
pub(crate) fn workload_is_sane(w: &pb::Workload) -> bool {
    let name_ok = !w.service_name.is_empty()
        && w.service_name.len() <= 63
        && w.service_name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    name_ok
        && w.deployment_id.parse::<Id>().is_ok()
        && w.environment_id.parse::<Id>().is_ok()
        && u16::try_from(w.port).is_ok_and(|p| p > 0)
}

impl Reconciler {
    pub fn new(
        runtime: Arc<dyn Runtime>,
        caps: Capabilities,
        out: mpsc::Sender<pb::NodeMessage>,
    ) -> Arc<Self> {
        let wake = caps.wake.filter(|_| caps.edge.is_some());
        Arc::new(Self {
            runtime,
            edge: caps.edge,
            build: caps.build,
            mesh: caps.mesh,
            access_log: caps.access_log,
            wake,
            functions: caps.functions,
            out,
            tracked: Mutex::new(HashMap::new()),
            applied: AtomicU64::new(0),
            tcp: TcpProxies::default(),
        })
    }

    /// Whether serverless deployments can sleep here (an edge and a wake proxy).
    pub fn serverless(&self) -> bool {
        self.wake.is_some()
    }

    pub fn applied_generation(&self) -> u64 {
        self.applied.load(Ordering::Relaxed)
    }

    /// The current phase of every deployment, re-sent after a reconnect so
    /// the server never misses a transition that happened while offline.
    pub fn replay(&self) -> Vec<pb::NodeMessage> {
        let tracked = self.tracked.lock().expect("tracked lock");
        tracked
            .iter()
            .filter(|(_, t)| {
                matches!(
                    t.phase,
                    Phase::Active | Phase::Failed | Phase::Crashed | Phase::Built | Phase::Sleeping
                )
            })
            .map(|(id, t)| event(id, t.phase, &t.message, self.applied_generation()))
            .collect()
    }

    pub async fn run(
        self: Arc<Self>,
        mut desired: watch::Receiver<Option<pb::DesiredState>>,
        stop: CancellationToken,
    ) {
        let mut tick = tokio::time::interval(REPAIR_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let woke = async {
                match &self.wake {
                    Some(w) => w.changed().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                _ = stop.cancelled() => break,
                changed = desired.changed() => if changed.is_err() { break },
                _ = tick.tick() => {}
                // A wake finished: the route goes back to the container now.
                _ = woke => {}
            }
            let current = desired.borrow_and_update().clone();
            if let Some(ds) = current
                && let Err(e) = self.apply(&ds).await
            {
                tracing::warn!(error = format!("{e:#}"), "reconcile failed; will retry");
            }
        }
        let tracked = self.tracked.lock().expect("tracked lock");
        for t in tracked.values() {
            if let Some(h) = &t.rollout {
                h.abort();
            }
        }
    }

    fn emit(&self, msg: pb::NodeMessage) {
        // Never block reconciliation on the network. Dropped events are
        // recovered by `replay` on the next connection.
        if self.out.try_send(msg).is_err() {
            tracing::debug!("outbox full; event dropped");
        }
    }

    fn set_phase(&self, deployment: &str, phase: Phase, message: impl Into<String>) {
        self.set_phase_with(deployment, phase, message, None, 0);
    }

    fn set_phase_with(
        &self,
        deployment: &str,
        phase: Phase,
        message: impl Into<String>,
        build: Option<pb::BuildOutput>,
        cold_start_ms: u32,
    ) {
        let message = message.into();
        {
            let mut tracked = self.tracked.lock().expect("tracked lock");
            let t = tracked
                .entry(deployment.to_owned())
                .or_insert(Tracked::new());
            t.phase = phase;
            t.message = message.clone();
            if phase == Phase::Active {
                t.active_since = tokio::time::Instant::now();
            }
            if !matches!(
                phase,
                Phase::Building
                    | Phase::Built
                    | Phase::Pulling
                    | Phase::Creating
                    | Phase::Starting
                    | Phase::Healthchecking
            ) {
                t.rollout = None;
            }
        }
        tracing::info!(deployment, phase = ?phase, %message, "deployment phase");
        let mut ev = event(deployment, phase, &message, self.applied_generation());
        if let Some(Msg::Event(e)) = &mut ev.msg {
            e.build = build;
            e.cold_start_ms = cold_start_ms;
        }
        self.emit(ev);
    }

    pub async fn apply(self: &Arc<Self>, ds: &pb::DesiredState) -> anyhow::Result<()> {
        let workloads: Vec<&pb::Workload> = ds
            .workloads
            .iter()
            .filter(|w| {
                let ok = workload_is_sane(w);
                if !ok {
                    tracing::warn!(deployment = %w.deployment_id, "ignoring malformed workload");
                }
                ok
            })
            .collect();
        let wanted: HashSet<&str> = workloads.iter().map(|w| w.deployment_id.as_str()).collect();
        let networks: HashSet<&str> = ds
            .networks
            .iter()
            .map(|n| n.environment_id.as_str())
            .collect();
        for net in &networks {
            self.runtime.ensure_network(net).await?;
        }
        let in_mesh = self.apply_mesh(ds).await;

        // 1. Point the edge at what serves now, *before* retiring anything, so
        //    traffic moves to a new deployment before the old one stops (and
        //    to the wake proxy before an idle serverless one stops).
        let containers = self.runtime.containers().await?;
        if let Some(w) = &self.wake {
            w.retain(|d| wanted.contains(d));
        }
        let sleepy = self.serverless_pass(ds, &workloads, &containers).await;
        self.apply_edge(ds, &containers).await;
        self.apply_tcp(ds, &containers).await;
        for (deployment, container) in sleepy {
            if let Err(e) = self.runtime.stop(&container).await {
                tracing::warn!(%deployment, error = format!("{e:#}"), "cannot stop an idle container");
            }
            if let Some(w) = &self.wake {
                w.asleep(&deployment);
            }
        }

        // 2. Stop what is no longer wanted.
        for c in containers
            .iter()
            .filter(|c| !wanted.contains(c.deployment_id.as_str()))
        {
            if self.rollout_running(&c.deployment_id) {
                continue; // removed below, after the task is aborted
            }
            self.runtime.remove(&c.id).await?;
            tracing::info!(deployment = %c.deployment_id, "removed container");
        }
        let stale: Vec<String> = {
            let mut tracked = self.tracked.lock().expect("tracked lock");
            let stale: Vec<String> = tracked
                .keys()
                .filter(|k| !wanted.contains(k.as_str()))
                .cloned()
                .collect();
            for k in &stale {
                if let Some(t) = tracked.remove(k)
                    && let Some(h) = t.rollout
                {
                    h.abort();
                }
            }
            stale
        };
        if !stale.is_empty() {
            for c in self.runtime.containers().await? {
                if stale.contains(&c.deployment_id) {
                    self.runtime.remove(&c.id).await?;
                }
            }
        }
        if let Some(f) = &self.functions {
            let functions: HashSet<&str> = workloads
                .iter()
                .filter(|w| w.function.is_some())
                .map(|w| w.deployment_id.as_str())
                .collect();
            f.retain(&functions);
        }

        // Data of deleted volumes. Only our own volumes, never anything else.
        for name in ds.volume_removals.iter().filter(|n| is_volume_name(n)) {
            match self.runtime.remove_volume(name).await {
                Ok(()) => tracing::info!(volume = %name, "removed volume"),
                Err(e) => {
                    tracing::warn!(volume = %name, error = format!("{e:#}"), "cannot remove volume yet")
                }
            }
        }

        // 3. Start, adopt, and watch what is wanted.
        let containers = self.runtime.containers().await?;
        if in_mesh {
            self.attach_to_mesh(&workloads, &containers).await;
        }
        for w in workloads {
            let mine: Vec<&Container> = containers
                .iter()
                .filter(|c| c.deployment_id == w.deployment_id)
                .collect();
            self.observe(w, &mine).await?;
        }

        // 4. Drop networks nobody uses any more.
        let used: HashSet<String> = self
            .runtime
            .containers()
            .await?
            .into_iter()
            .map(|c| c.environment_id)
            .collect();
        for net in self.runtime.networks().await? {
            if !networks.contains(net.as_str()) && !used.contains(&net) {
                self.runtime.remove_network(&net).await?;
            }
        }

        self.applied.store(ds.generation, Ordering::Relaxed);
        let report = pb::StateReport {
            generation: ds.generation,
            workloads: self
                .runtime
                .containers()
                .await?
                .into_iter()
                .map(|c| pb::WorkloadStatus {
                    deployment_id: c.deployment_id,
                    container_id: c.id,
                    running: c.running,
                    healthy: c.running,
                    exit_code: c.exit_code as i32,
                    restart_count: c.restart_count,
                })
                .collect(),
            edge_unsupported: self
                .edge
                .as_ref()
                .map(|e| e.unsupported())
                .unwrap_or_default(),
        };
        self.emit(pb::NodeMessage {
            msg: Some(Msg::Report(report)),
        });
        Ok(())
    }

    /// Converge the mesh (network, tunnels, firewall, resolver) when the
    /// server put this node in one. False when it isn't (or that failed):
    /// workloads then run without mesh addresses.
    async fn apply_mesh(self: &Arc<Self>, ds: &pb::DesiredState) -> bool {
        let (Some(mesh), Some(spec)) = (&self.mesh, &ds.mesh) else {
            return false;
        };
        let Some(subnet) = crate::mesh::Subnet::parse(&spec.subnet) else {
            tracing::warn!(subnet = %spec.subnet, "ignoring an invalid mesh subnet");
            return false;
        };
        // The bridge first: it holds the address the tunnel and resolver use.
        if let Err(e) = self
            .runtime
            .ensure_mesh_network(subnet, crate::mesh::Mesh::mtu())
            .await
        {
            tracing::warn!(
                error = format!("{e:#}"),
                "cannot create the mesh network; will retry"
            );
            return false;
        }
        if let Err(e) = mesh.apply(spec).await {
            tracing::warn!(
                error = format!("{e:#}"),
                "cannot configure the mesh; will retry"
            );
        }
        true
    }

    /// Containers from before this node was in a mesh join it in place.
    async fn attach_to_mesh(&self, workloads: &[&pb::Workload], containers: &[Container]) {
        for w in workloads {
            let Ok(ip) = w.mesh_ip.parse::<std::net::Ipv4Addr>() else {
                continue;
            };
            for c in containers
                .iter()
                .filter(|c| c.deployment_id == w.deployment_id && c.mesh_ip.is_none())
            {
                match self.runtime.attach_mesh(&c.id, ip).await {
                    Ok(()) => {
                        tracing::info!(deployment = %w.deployment_id, %ip, "attached to the mesh")
                    }
                    Err(e) => tracing::warn!(
                        deployment = %w.deployment_id,
                        error = format!("{e:#}"),
                        "cannot attach to the mesh"
                    ),
                }
            }
        }
    }

    /// Serverless bookkeeping before the edge is updated: a woken deployment
    /// is ACTIVE again (its route goes back to the container), and an idle
    /// one is SLEEPING (its route moves to the wake proxy). Returns the
    /// (deployment, container) pairs to stop once the edge has moved. Only
    /// deployments this node routes sleep: nothing else could wake them.
    async fn serverless_pass(
        &self,
        ds: &pb::DesiredState,
        workloads: &[&pb::Workload],
        containers: &[Container],
    ) -> Vec<(String, String)> {
        let (Some(wake), Some(edge)) = (&self.wake, &self.edge) else {
            return Vec::new();
        };
        let now = tokio::time::Instant::now();
        let mut idle = Vec::new();
        for w in workloads
            .iter()
            .filter(|w| w.serverless.is_some() && !w.cron)
        {
            let id = w.deployment_id.as_str();
            if self.rollout_running(id) {
                continue;
            }
            let running = containers
                .iter()
                .find(|c| c.deployment_id == id && c.running);
            match self.phase_of(id) {
                Some(Phase::Sleeping)
                    if running.is_some() && matches!(wake.state(id), Some(WakeState::Awake(_))) =>
                {
                    let ms = wake
                        .take_cold_start(id)
                        .map_or(0, |d| u32::try_from(d.as_millis()).unwrap_or(u32::MAX));
                    self.set_phase_with(
                        id,
                        Phase::Active,
                        format!("Woke up on a request (cold start {ms} ms)"),
                        None,
                        ms.max(1),
                    );
                }
                Some(Phase::Active) if ds.routes.iter().any(|r| r.deployment_id == id) => {
                    let Some(c) = running else { continue };
                    let secs = w.serverless.as_ref().map_or(0, |s| s.idle_secs).max(1);
                    let last = [
                        self.active_since(id),
                        self.access_log.as_ref().and_then(|l| l.last_request(id)),
                        wake.last_active(id),
                    ]
                    .into_iter()
                    .flatten()
                    .max()
                    .unwrap_or(now);
                    if now.duration_since(last) >= Duration::from_secs(u64::from(secs)) {
                        idle.push((*w, c, secs));
                    }
                }
                _ => {}
            }
        }
        if idle.is_empty() {
            return Vec::new();
        }
        // Requests still in flight (a websocket, a long poll) keep it awake.
        let in_flight = edge.in_flight().await;
        let mut out = Vec::new();
        for (w, c, secs) in idle {
            let id = w.deployment_id.as_str();
            if let Ok(Some(ip)) = self.runtime.address(&c.id, &c.environment_id).await
                && in_flight.iter().any(|(a, n)| a.ip() == ip && *n > 0)
            {
                if let Some(t) = self.tracked.lock().expect("tracked lock").get_mut(id) {
                    t.active_since = now;
                }
                continue;
            }
            wake.draining(id, Arc::new(self.wake_target(w, &c.id)));
            self.set_phase(
                id,
                Phase::Sleeping,
                format!("Asleep after {} without requests", idle_text(secs)),
            );
            out.push((id.to_owned(), c.id.clone()));
        }
        out
    }

    /// How the wake proxy starts `container` of `w` again.
    fn wake_target(&self, w: &pb::Workload, container: &str) -> ContainerTarget {
        let health = w.health.clone().unwrap_or_default();
        let limit = if health.path.is_empty() {
            crate::edge::wake::MAX_WAKE
        } else {
            Duration::from_secs(u64::from(health.timeout_secs.max(1)))
                .min(crate::edge::wake::MAX_WAKE)
        };
        ContainerTarget {
            runtime: self.runtime.clone(),
            container_id: container.to_owned(),
            environment_id: w.environment_id.clone(),
            port: u16::try_from(w.port).unwrap_or(0),
            health_path: health.path,
            timeout: limit,
        }
    }

    fn active_since(&self, deployment: &str) -> Option<tokio::time::Instant> {
        self.tracked
            .lock()
            .expect("tracked lock")
            .get(deployment)
            .map(|t| t.active_since)
    }

    /// Resolve routes to running containers and hand them to the edge. Edge
    /// failures are logged, never fatal: workloads keep reconciling.
    async fn apply_edge(&self, ds: &pb::DesiredState, containers: &[Container]) {
        let Some(edge) = &self.edge else { return };
        // Sleeping deployments are served by the wake proxy.
        let sleeping: HashSet<String> = match &self.wake {
            Some(_) => self
                .tracked
                .lock()
                .expect("tracked lock")
                .iter()
                .filter(|(_, t)| t.phase == Phase::Sleeping)
                .map(|(d, _)| d.clone())
                .collect(),
            None => HashSet::new(),
        };
        let mut routes = Vec::new();
        for r in &ds.routes {
            // A function: its socket in this node's workerd.
            if let Some(port) = self
                .functions
                .as_ref()
                .and_then(|f| f.port(&r.deployment_id))
            {
                routes.push(ResolvedRoute {
                    hostname: r.hostname.clone(),
                    upstream: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
                });
                continue;
            }
            let Ok(port) = u16::try_from(r.port) else {
                continue;
            };
            if let Some(w) = self
                .wake
                .as_ref()
                .filter(|_| sleeping.contains(&r.deployment_id))
            {
                routes.push(ResolvedRoute {
                    hostname: r.hostname.clone(),
                    upstream: w.addr(),
                });
                continue;
            }
            for c in containers
                .iter()
                .filter(|c| c.deployment_id == r.deployment_id && c.running)
            {
                match self.runtime.address(&c.id, &c.environment_id).await {
                    Ok(Some(ip)) => routes.push(ResolvedRoute {
                        hostname: r.hostname.clone(),
                        upstream: std::net::SocketAddr::new(ip, port),
                    }),
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = format!("{e:#}"), "cannot resolve a route upstream")
                    }
                }
            }
            // Replicas on other servers, over the mesh (only inside it).
            if ds.mesh.is_some() {
                for up in &r.remote_upstreams {
                    match up.parse::<std::net::SocketAddr>() {
                        Ok(a) if matches!(a.ip(), std::net::IpAddr::V4(v4) if v4.octets()[..2] == [10, 80]) => {
                            routes.push(ResolvedRoute {
                                hostname: r.hostname.clone(),
                                upstream: a,
                            })
                        }
                        _ => {
                            tracing::warn!(upstream = %up, "ignoring a remote upstream outside the mesh")
                        }
                    }
                }
            }
        }
        let sites = ds
            .routes
            .iter()
            .filter_map(|r| {
                let host = crate::edge::normalize_host(&r.hostname)?;
                Some((
                    host,
                    Site {
                        deployment_id: r.deployment_id.clone(),
                        proxied: r.proxied,
                        rules: r.rules.clone(),
                        wake: self
                            .wake
                            .as_ref()
                            .filter(|_| sleeping.contains(&r.deployment_id))
                            .map(|w| w.header(&r.deployment_id, r.port as u16)),
                    },
                ))
            })
            .collect();
        let settings = ds.edge.clone().unwrap_or_default();
        let cfg = EdgeConfig {
            routes,
            sites,
            tls: pb::TlsMode::try_from(settings.tls).unwrap_or(pb::TlsMode::TlsAcme),
            acme_email: settings.acme_email,
            dashboard_hosts: settings.dashboard_hosts,
        };
        if let Err(e) = edge.apply(&cfg).await {
            tracing::warn!(error = format!("{e:#}"), "edge update failed; will retry");
        }
    }

    /// Point each TCP proxy at its deployment's running container.
    async fn apply_tcp(&self, ds: &pb::DesiredState, containers: &[Container]) {
        let mut wanted = HashMap::new();
        for p in &ds.tcp_proxies {
            let (Ok(public), Ok(target)) =
                (u16::try_from(p.public_port), u16::try_from(p.target_port))
            else {
                continue;
            };
            let mut upstream = None;
            if let Some(c) = containers
                .iter()
                .find(|c| c.deployment_id == p.deployment_id && c.running)
            {
                match self.runtime.address(&c.id, &c.environment_id).await {
                    Ok(ip) => upstream = ip.map(|ip| std::net::SocketAddr::new(ip, target)),
                    Err(e) => tracing::warn!(
                        error = format!("{e:#}"),
                        "cannot resolve a tcp proxy upstream"
                    ),
                }
            }
            wanted.insert(public, upstream);
        }
        self.tcp.apply(&wanted).await;
    }

    fn rollout_running(&self, deployment: &str) -> bool {
        let tracked = self.tracked.lock().expect("tracked lock");
        tracked
            .get(deployment)
            .and_then(|t| t.rollout.as_ref())
            .is_some_and(|h| !h.is_finished())
    }

    /// Whether the server asked for a restart this node hasn't done yet. The
    /// first generation seen is taken as applied (it may predate a node restart).
    fn restart_due(&self, deployment: &str, generation: u32) -> bool {
        let mut tracked = self.tracked.lock().expect("tracked lock");
        let Some(t) = tracked.get_mut(deployment) else {
            return false;
        };
        match t.restarted {
            Some(g) if g >= generation => false,
            Some(_) => {
                t.restarted = Some(generation);
                true
            }
            None => {
                t.restarted = Some(generation);
                false
            }
        }
    }

    fn phase_of(&self, deployment: &str) -> Option<Phase> {
        self.tracked
            .lock()
            .expect("tracked lock")
            .get(deployment)
            .map(|t| t.phase)
    }

    async fn observe(
        self: &Arc<Self>,
        w: &pb::Workload,
        containers: &[&Container],
    ) -> anyhow::Result<()> {
        let id = w.deployment_id.as_str();
        if self.rollout_running(id) {
            return Ok(());
        }
        if w.function.is_some() {
            self.observe_function(w);
            return Ok(());
        }
        let phase = self.phase_of(id);
        if phase == Some(Phase::Failed) {
            return Ok(()); // the server removes it from the desired state
        }
        if phase == Some(Phase::Built) && exports_output(w) {
            return Ok(()); // uploaded; the server takes it from here
        }
        // A cron service has no container: ready is all there is to watch.
        if w.cron && phase == Some(Phase::Active) {
            return Ok(());
        }
        let serverless = w.serverless.is_some() && !w.cron && self.wake.is_some();
        if serverless && phase == Some(Phase::Sleeping) {
            // Its next start is the restart.
            self.restart_due(id, w.restart_generation);
            let waking = self
                .wake
                .as_ref()
                .and_then(|wk| wk.state(id))
                .is_some_and(|s| matches!(s, WakeState::Waking | WakeState::Draining));
            match containers.first() {
                None => {
                    if let Some(wk) = &self.wake {
                        wk.retain(|d| d != id);
                    }
                    self.start_rollout(w.clone());
                }
                // Started by something else (Docker, after its own restart).
                Some(c) if c.running && !waking => self.set_phase(id, Phase::Active, "running"),
                Some(_) => {}
            }
            return Ok(());
        }
        // Extra containers for one deployment can only come from a crash
        // mid-rollout; keep the first.
        for extra in containers.iter().skip(1) {
            self.runtime.remove(&extra.id).await?;
        }
        if let Some(c) = containers.first()
            && self.restart_due(id, w.restart_generation)
        {
            self.runtime.restart(&c.id).await?;
            self.set_phase(id, Phase::Active, "restarted");
            return Ok(());
        }
        match (containers.first(), phase) {
            (None, _) => self.start_rollout(w.clone()),
            // Adopt a container that survived a node restart.
            (Some(c), None) if c.running => self.set_phase(id, Phase::Active, "running"),
            // A serverless one stopped: it was asleep.
            (Some(c), None) if serverless => {
                if let Some(wk) = &self.wake {
                    wk.draining(id, Arc::new(self.wake_target(w, &c.id)));
                    wk.asleep(id);
                }
                self.set_phase(id, Phase::Sleeping, "Asleep");
            }
            (Some(c), None) => self.set_phase(
                id,
                Phase::Crashed,
                format!("exited with code {}", c.exit_code),
            ),
            (Some(c), Some(Phase::Active)) if !c.running => self.set_phase(
                id,
                Phase::Crashed,
                format!("exited with code {}", c.exit_code),
            ),
            (Some(c), Some(Phase::Crashed)) if c.running => {
                self.set_phase(id, Phase::Active, "recovered")
            }
            (Some(_), _) => {}
        }
        Ok(())
    }

    /// A function: rolled out once, then watched through the functions
    /// runtime (every change to the node's functions restarts it, so only
    /// a longer outage is a crash).
    fn observe_function(self: &Arc<Self>, w: &pb::Workload) {
        let id = w.deployment_id.as_str();
        let phase = self.phase_of(id);
        if phase == Some(Phase::Failed) {
            return;
        }
        let Some(fns) = &self.functions else {
            self.set_phase(id, Phase::Failed, NO_FUNCTIONS);
            return;
        };
        if !fns.contains(id) {
            self.start_rollout(w.clone());
            return;
        }
        if self.restart_due(id, w.restart_generation) {
            fns.restart();
            self.set_phase(id, Phase::Active, "restarted");
            return;
        }
        let down = fns.down_for() > FUNCTIONS_DOWN_IS_A_CRASH;
        match phase {
            Some(Phase::Active) if down => self.set_phase(
                id,
                Phase::Crashed,
                "The functions runtime on this server stopped; restarting it",
            ),
            Some(Phase::Crashed) if fns.up() => self.set_phase(id, Phase::Active, "recovered"),
            _ => {}
        }
    }

    fn start_rollout(self: &Arc<Self>, w: pb::Workload) {
        let this = self.clone();
        let id = w.deployment_id.clone();
        let handle = tokio::spawn(async move {
            let id = w.deployment_id.clone();
            if let Err(message) = this.rollout(&w).await {
                // Clean up whatever was created; the deployment is not retried.
                if let Ok(cs) = this.runtime.containers().await {
                    for c in cs.iter().filter(|c| c.deployment_id == id) {
                        let _ = this.runtime.remove(&c.id).await;
                    }
                }
                this.set_phase(&id, Phase::Failed, message);
            }
        });
        let mut tracked = self.tracked.lock().expect("tracked lock");
        let t = tracked.entry(id).or_insert(Tracked::new());
        t.rollout = Some(handle);
    }

    async fn rollout(&self, w: &pb::Workload) -> Result<(), String> {
        let id = w.deployment_id.as_str();
        if let Some(f) = &w.function {
            return self.rollout_function(w, f).await;
        }
        let mut effective;
        let w = match &w.build {
            Some(spec) if !spec.output_dir.is_empty() => {
                let built = self.build(w, spec).await?;
                let sha256 = self.upload_output(w, spec).await?;
                // The message is the archive's hash: the server checks it.
                self.set_phase_with(
                    id,
                    Phase::Built,
                    sha256,
                    Some(pb::BuildOutput {
                        stack: built.stack,
                        ..Default::default()
                    }),
                    0,
                );
                return Ok(());
            }
            Some(spec) => {
                let built = self.build(w, spec).await?;
                // The message is the image id: the server stores it as the digest.
                self.set_phase_with(
                    id,
                    Phase::Built,
                    built.image,
                    Some(pb::BuildOutput {
                        stack: built.stack.clone(),
                        ..built.overrides.to_output()
                    }),
                    0,
                );
                // The repository's config file overrides the dashboard.
                effective = w.clone();
                built.overrides.apply(&mut effective);
                &effective
            }
            None => w,
        };
        // Built images exist only here; everything else comes from a registry.
        if !w.image.starts_with(LOCAL_PREFIX) {
            self.set_phase(id, Phase::Pulling, format!("Pulling {}", w.image));
            self.runtime
                .pull(&w.image)
                .await
                .map_err(|e| format!("Image pull failed: {e:#}"))?;
        }

        // Before anything of this deployment starts (and before a volume
        // service's old container stops): a failure leaves the serving one alone.
        if !w.pre_deploy_command.is_empty() {
            self.pre_deploy(w).await?;
        }
        // Runs come later, one at a time.
        if w.cron {
            self.set_phase(id, Phase::Active, "Ready: runs on its cron schedule");
            return Ok(());
        }

        // Two containers must never share a volume's data (a database would
        // corrupt it): stop any other deployment of this service first.
        if w.volume.is_some() {
            let others = self
                .runtime
                .containers()
                .await
                .map_err(|e| format!("{e:#}"))?;
            for c in others
                .iter()
                .filter(|c| c.service_id == w.service_id && c.deployment_id != w.deployment_id)
            {
                self.set_phase(
                    id,
                    Phase::Creating,
                    "Stopping the previous deployment (volume)",
                );
                self.runtime
                    .remove(&c.id)
                    .await
                    .map_err(|e| format!("Could not stop the previous deployment: {e:#}"))?;
            }
        }
        self.set_phase(id, Phase::Creating, "Creating container");
        let container = self
            .runtime
            .create(w)
            .await
            .map_err(|e| format!("Could not create the container: {e:#}"))?;

        self.set_phase(id, Phase::Starting, "Starting container");
        self.runtime
            .start(&container)
            .await
            .map_err(|e| format!("Could not start the container: {e:#}"))?;

        let health = w.health.clone().unwrap_or_default();
        let port = u16::try_from(w.port).unwrap_or(0);
        if health.path.is_empty() {
            tokio::time::sleep(SETTLE).await;
            self.ensure_running(&container, w).await?;
        } else {
            self.set_phase(
                id,
                Phase::Healthchecking,
                format!("Waiting for {} to return 2xx", health.path),
            );
            let deadline = tokio::time::Instant::now()
                + Duration::from_secs(u64::from(health.timeout_secs.max(1)));
            loop {
                self.ensure_running(&container, w).await?;
                if self
                    .runtime
                    .probe(&container, &w.environment_id, port, &health.path)
                    .await
                    .unwrap_or(false)
                {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    let outcome = format!(
                        "Health check {} did not pass within {}s",
                        health.path, health.timeout_secs
                    );
                    return Err(self.with_output(outcome, &container, w).await);
                }
                tokio::time::sleep(PROBE_EVERY).await;
            }
        }
        self.set_phase(id, Phase::Active, "Deployment is live");
        Ok(())
    }

    /// A function: its code (inline, bundled before, or bundled now), a
    /// start of its own, then a place in this node's workerd.
    async fn rollout_function(&self, w: &pb::Workload, f: &pb::Function) -> Result<(), String> {
        let id = w.deployment_id.as_str();
        let fns = self.functions.as_ref().ok_or(NO_FUNCTIONS)?;
        let code = if !f.code.is_empty() {
            f.code.clone()
        } else if let Some(code) = fns.saved_code(id) {
            code
        } else {
            let spec = w.build.as_ref().ok_or("The function has no code to run")?;
            self.build(w, spec).await?;
            let code = self.bundle(w).await?;
            self.set_phase(
                id,
                Phase::Built,
                format!("Bundled the function ({} KB)", code.len().div_ceil(1024)),
            );
            code
        };
        self.set_phase(id, Phase::Starting, "Starting the function");
        let spec = crate::functions::FunctionSpec::new(w, f, code);
        fns.check(&spec).await.map_err(|e| redact(&e, &w.env))?;
        let version = fns
            .upsert(spec)
            .map_err(|e| format!("Cannot add the function: {e}"))?;
        fns.wait_serving(version, FUNCTION_START_TIMEOUT).await?;
        let health = w.health.clone().unwrap_or_default();
        if !health.path.is_empty() {
            let port = fns.port(id).ok_or("The function was removed meanwhile")?;
            self.set_phase(
                id,
                Phase::Healthchecking,
                format!("Waiting for {} to return 2xx", health.path),
            );
            let url = format!("http://127.0.0.1:{port}{}", health.path);
            let _ = rustls::crypto::ring::default_provider().install_default();
            let deadline = tokio::time::Instant::now()
                + Duration::from_secs(u64::from(health.timeout_secs.max(1)));
            loop {
                let ok = reqwest::Client::new()
                    .get(&url)
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .is_ok_and(|r| r.status().is_success());
                if ok {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(format!(
                        "Health check {} did not pass within {}s",
                        health.path, health.timeout_secs
                    ));
                }
                tokio::time::sleep(PROBE_EVERY).await;
            }
        }
        self.set_phase(id, Phase::Active, "Function is live");
        Ok(())
    }

    /// The bundle a FUNCTION build made: its image's one module.
    async fn bundle(&self, w: &pb::Workload) -> Result<String, String> {
        let id = w.deployment_id.as_str();
        let b = self
            .build
            .as_ref()
            .ok_or("This server can't build (no builder configured)")?;
        let log = b
            .logs
            .append(id)
            .map_err(|e| format!("Cannot write the build log: {e:#}"))?;
        let _finish = FinishOnDrop(log.clone());
        let archive = std::env::temp_dir().join(format!("modula-function-{id}.tar.gz"));
        let result = async {
            b.builder
                .export(w, pb::FUNCTION_OUTPUT_DIR, &archive, &log)
                .await?;
            let path = archive.clone();
            tokio::task::spawn_blocking(move || {
                crate::build::output::read_file(&path, pb::FUNCTION_MODULE)
            })
            .await?
        }
        .await;
        let _ = tokio::fs::remove_file(&archive).await;
        result.map_err(|e| {
            let message = format!("{e:#}");
            log.line(true, &message);
            format!("Bundling the function failed: {message}")
        })
    }

    /// `outcome`, followed by the container's last output (redacted): the
    /// container is removed after a failed start, and with it its logs.
    async fn with_output(&self, outcome: String, container: &str, w: &pb::Workload) -> String {
        use futures::StreamExt as _;
        let lines = tokio::time::timeout(Duration::from_secs(3), async {
            match self.runtime.logs(container, 20, false).await {
                Ok(stream) => {
                    stream
                        .filter_map(|l| async move { l.ok().map(|l| l.text) })
                        .collect::<Vec<_>>()
                        .await
                }
                Err(_) => Vec::new(),
            }
        })
        .await
        .unwrap_or_default();
        let output = redact(lines.join("\n").trim_end(), &w.env);
        let cut = output.len().saturating_sub(PRE_DEPLOY_TAIL);
        let cut = (cut..=output.len())
            .find(|i| output.is_char_boundary(*i))
            .unwrap_or(output.len());
        match &output[cut..] {
            "" => outcome,
            out => format!("{outcome}:\n{out}"),
        }
    }

    /// Run the pre-deploy command to completion. Its output goes to the
    /// deployment's build log; the tail of it into the error when it fails.
    async fn pre_deploy(&self, w: &pb::Workload) -> Result<(), String> {
        let id = w.deployment_id.as_str();
        let shown: String = w.pre_deploy_command.join(" ").chars().take(200).collect();
        self.set_phase(
            id,
            Phase::Creating,
            format!("Running pre-deploy command: {shown}"),
        );
        let log = self.build.as_ref().and_then(|b| b.logs.append(id).ok());
        let _finish = log.clone().map(FinishOnDrop);
        if let Some(l) = &log {
            l.line(false, &format!("Running pre-deploy command: {shown}"));
        }
        let tail = Arc::new(Mutex::new(std::collections::VecDeque::<String>::new()));
        let sink = {
            let (tail, log) = (tail.clone(), log.clone());
            Box::new(move |line: crate::runtime::LogLine| {
                if let Some(l) = &log {
                    l.line(line.stderr, &line.text);
                }
                let mut t = tail.lock().expect("tail lock");
                t.push_back(line.text);
                while t.len() > 20 {
                    t.pop_front();
                }
            })
        };
        // On the mesh with a leased address, so it reaches services on other
        // servers (migrations against a database elsewhere).
        let lease = match &self.mesh {
            Some(m) if !w.mesh_ip.is_empty() => m.lease(&w.environment_id).await,
            _ => None,
        };
        let mut once = w.clone();
        once.mesh_ip = lease.as_ref().map(|l| l.ip.to_string()).unwrap_or_default();
        let result = self
            .runtime
            .run_once(&once, &w.pre_deploy_command, PRE_DEPLOY_TIMEOUT, sink)
            .await;
        drop(lease);
        let outcome = match &result {
            Ok(0) => return Ok(()),
            Ok(code) => format!("Pre-deploy command exited with code {code}"),
            Err(e) => format!("Pre-deploy command failed: {e:#}"),
        };
        if let Some(l) = &log {
            l.line(true, &outcome);
        }
        let output = tail
            .lock()
            .expect("tail lock")
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        let output = redact(&output, &w.env);
        let cut = output.len().saturating_sub(PRE_DEPLOY_TAIL);
        let cut = (cut..=output.len())
            .find(|i| output.is_char_boundary(*i))
            .unwrap_or(output.len());
        Err(match &output[cut..] {
            "" => outcome,
            out => format!("{outcome}:\n{out}"),
        })
    }

    /// Build the workload's image.
    async fn build(&self, w: &pb::Workload, spec: &pb::Build) -> Result<Built, String> {
        let id = w.deployment_id.as_str();
        let Some(b) = &self.build else {
            return Err("This server can't build images (no builder configured)".into());
        };
        self.set_phase(id, Phase::Building, "Building from source");
        let log = b
            .logs
            .create(id)
            .map_err(|e| format!("Cannot write the build log: {e:#}"))?;
        // Followers stop waiting even if the build is cancelled mid-way.
        let _finish = FinishOnDrop(log.clone());
        match b.builder.build(w, spec, &log).await {
            Ok(image) => {
                log.line(false, "Build complete");
                Ok(image)
            }
            Err(e) => {
                let message = format!("{e:#}");
                log.line(true, &message);
                Err(format!("Build failed: {message}"))
            }
        }
    }

    /// Packs the build's output directory and uploads it to the control
    /// plane. Returns the archive's hex SHA-256.
    async fn upload_output(&self, w: &pb::Workload, spec: &pb::Build) -> Result<String, String> {
        let id = w.deployment_id.as_str();
        let Some(b) = &self.build else {
            return Err("This server can't build images (no builder configured)".into());
        };
        let log = b.logs.append(id).ok();
        let _finish = log.clone().map(FinishOnDrop);
        let archive = std::env::temp_dir().join(format!("modula-output-{id}.tar.gz"));
        let result = async {
            let log = log
                .clone()
                .ok_or_else(|| anyhow::anyhow!("cannot write the build log"))?;
            b.builder
                .export(w, &spec.output_dir, &archive, &log)
                .await?;
            let sha256 = crate::build::output::sha256(&archive).await?;
            let size = tokio::fs::metadata(&archive).await?.len();
            log.line(
                false,
                &format!("Uploading the output ({} KB) for publishing", size / 1024),
            );
            crate::build::output::upload(&spec.output_url, &spec.output_auth, &archive).await?;
            log.line(false, "Output uploaded");
            anyhow::Ok(sha256)
        }
        .await;
        let _ = tokio::fs::remove_file(&archive).await;
        result.map_err(|e| {
            let message = format!("{e:#}");
            if let Some(l) = &log {
                l.line(true, &message);
            }
            format!("Packing the build output failed: {message}")
        })
    }

    async fn ensure_running(&self, container: &str, w: &pb::Workload) -> Result<(), String> {
        let cs = self
            .runtime
            .containers()
            .await
            .map_err(|e| format!("{e:#}"))?;
        match cs.iter().find(|c| c.id == container) {
            Some(c) if c.running => Ok(()),
            Some(c) => {
                let outcome = format!("The container exited during startup (code {})", c.exit_code);
                Err(self.with_output(outcome, container, w).await)
            }
            None => Err("The container disappeared during startup".into()),
        }
    }
}

/// The server's `deploy::env::worth_masking`: a credential-looking name, or a
/// long value mixing letters and digits.
fn worth_masking(e: &pb::EnvVar) -> bool {
    const SECRET_WORDS: [&str; 8] = [
        "PASSWORD",
        "PASSWD",
        "SECRET",
        "TOKEN",
        "KEY",
        "PRIVATE",
        "CREDENTIAL",
        "AUTH",
    ];
    let (name, value) = (e.name.to_ascii_uppercase(), e.value.as_str());
    if value.len() < 6 {
        return false;
    }
    SECRET_WORDS.iter().any(|w| name.contains(w))
        || (value.len() >= 12
            && value.bytes().any(|b| b.is_ascii_alphabetic())
            && value.bytes().any(|b| b.is_ascii_digit())
            && !value.contains(char::is_whitespace))
}

/// Masks the workload's variable values in text that leaves the node outside
/// the log stream (which the server redacts itself), e.g. a failure message.
fn redact(text: &str, env: &[pb::EnvVar]) -> String {
    let mut out = text.to_owned();
    let mut secrets: Vec<&str> = env
        .iter()
        .filter(|e| !e.name.starts_with("MODULA_") && e.name != "PORT" && worth_masking(e))
        .map(|e| e.value.as_str())
        .collect();
    // Longest first, so a value containing another is masked whole.
    secrets.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for v in secrets {
        out = out.replace(v, "••••••");
    }
    out
}

/// `secs` for humans: `45 s`, `10 min`, `2 h`.
fn idle_text(secs: u32) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}
const NO_FUNCTIONS: &str = "This server can't run functions (workerd isn't installed)";
/// workerd must serve a new function within this.
const FUNCTION_START_TIMEOUT: Duration = Duration::from_secs(60);
/// The functions runtime down this long counts as a crash of its functions.
const FUNCTIONS_DOWN_IS_A_CRASH: Duration = Duration::from_secs(15);

/// A static-site build whose output is hosted elsewhere.
fn exports_output(w: &pb::Workload) -> bool {
    w.build.as_ref().is_some_and(|b| !b.output_dir.is_empty())
}

struct FinishOnDrop(BuildLog);

impl Drop for FinishOnDrop {
    fn drop(&mut self) {
        self.0.finish();
    }
}

fn event(deployment: &str, phase: Phase, message: &str, generation: u64) -> pb::NodeMessage {
    pb::NodeMessage {
        msg: Some(Msg::Event(pb::DeploymentEvent {
            deployment_id: deployment.to_owned(),
            phase: phase as i32,
            message: message.to_owned(),
            at_unix_ms: now_ms(),
            generation,
            build: None,
            cold_start_ms: 0,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeRuntime;

    fn workload(dep: Id, env: Id, health: &str) -> pb::Workload {
        pb::Workload {
            deployment_id: dep.to_string(),
            service_id: Id::ZERO.to_string(),
            environment_id: env.to_string(),
            service_name: "web".into(),
            image: "nginx:alpine".into(),
            port: 80,
            health: Some(pb::HealthCheck {
                path: health.into(),
                timeout_secs: 2,
            }),
            ..Default::default()
        }
    }

    fn state(generation: u64, env: Id, ws: Vec<pb::Workload>) -> pb::DesiredState {
        pb::DesiredState {
            generation,
            networks: vec![pb::Network {
                environment_id: env.to_string(),
            }],
            workloads: ws,
            ..Default::default()
        }
    }

    /// Every event emitted so far, as (deployment, phase).
    fn drain(rx: &mut mpsc::Receiver<pb::NodeMessage>) -> Vec<(String, Phase)> {
        let mut out = vec![];
        while let Ok(m) = rx.try_recv() {
            if let Some(Msg::Event(e)) = m.msg {
                out.push((e.deployment_id, Phase::try_from(e.phase).unwrap()));
            }
        }
        out
    }

    fn of(events: &[(String, Phase)], dep: Id) -> Vec<Phase> {
        events
            .iter()
            .filter(|(d, _)| *d == dep.to_string())
            .map(|(_, p)| *p)
            .collect()
    }

    /// Drain and return the phases emitted for `dep`.
    fn phases(rx: &mut mpsc::Receiver<pb::NodeMessage>, dep: Id) -> Vec<Phase> {
        of(&drain(rx), dep)
    }

    async fn settle(r: &Arc<Reconciler>, dep: Id) {
        for _ in 0..50 {
            if !r.rollout_running(&dep.to_string()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("rollout did not finish");
    }

    #[tokio::test(start_paused = true)]
    async fn a_higher_restart_generation_restarts_in_place() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), Capabilities::default(), tx);
        let (env, dep) = (Id::new(), Id::new());
        let mut w = workload(dep, env, "");
        r.apply(&state(1, env, vec![w.clone()])).await.unwrap();
        settle(&r, dep).await;
        assert_eq!(phases(&mut rx, dep).last(), Some(&Phase::Active));
        let restarts = || async { rt.containers().await.unwrap()[0].restart_count };

        // Seen again unchanged: nothing happens.
        r.apply(&state(2, env, vec![w.clone()])).await.unwrap();
        assert_eq!(restarts().await, 0);

        w.restart_generation = 1;
        r.apply(&state(3, env, vec![w.clone()])).await.unwrap();
        assert_eq!(restarts().await, 1);
        r.apply(&state(4, env, vec![w])).await.unwrap();
        assert_eq!(restarts().await, 1, "a generation is applied once");
    }

    #[tokio::test(start_paused = true)]
    async fn rolls_out_supersedes_and_removes() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), Capabilities::default(), tx);
        let (env, d1, d2) = (Id::new(), Id::new(), Id::new());

        r.apply(&state(1, env, vec![workload(d1, env, "/health")]))
            .await
            .unwrap();
        settle(&r, d1).await;
        assert_eq!(
            phases(&mut rx, d1),
            [
                Phase::Pulling,
                Phase::Creating,
                Phase::Starting,
                Phase::Healthchecking,
                Phase::Active
            ]
        );
        assert_eq!(rt.running_deployments(), [d1.to_string()].into());
        assert_eq!(r.applied_generation(), 1);

        // Both run while the new one comes up (zero downtime) ...
        r.apply(&state(
            2,
            env,
            vec![workload(d1, env, "/health"), workload(d2, env, "")],
        ))
        .await
        .unwrap();
        settle(&r, d2).await;
        assert_eq!(rt.running_deployments().len(), 2);
        // ... then the server drops the old one.
        r.apply(&state(3, env, vec![workload(d2, env, "")]))
            .await
            .unwrap();
        assert_eq!(rt.running_deployments(), [d2.to_string()].into());

        // Nothing desired: containers and the network go away.
        r.apply(&pb::DesiredState {
            generation: 4,
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(rt.running_deployments().is_empty());
        assert!(rt.network_ids().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_reported_once_and_cleaned_up() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), Capabilities::default(), tx);
        let (env, bad_pull, unhealthy) = (Id::new(), Id::new(), Id::new());
        rt.unpullable
            .lock()
            .unwrap()
            .insert("ghcr.io/nope/missing:1".into());
        rt.unhealthy.lock().unwrap().insert(unhealthy.to_string());
        let mut w1 = workload(bad_pull, env, "");
        w1.image = "ghcr.io/nope/missing:1".into();
        let ds = state(1, env, vec![w1, workload(unhealthy, env, "/health")]);

        r.apply(&ds).await.unwrap();
        settle(&r, bad_pull).await;
        settle(&r, unhealthy).await;
        let events = drain(&mut rx);
        assert_eq!(of(&events, bad_pull), [Phase::Pulling, Phase::Failed]);
        assert_eq!(of(&events, unhealthy).last(), Some(&Phase::Failed));
        assert!(
            rt.running_deployments().is_empty(),
            "failed containers are removed"
        );

        // Re-applying the same state does not retry failed deployments.
        r.apply(&ds).await.unwrap();
        assert!(phases(&mut rx, bad_pull).is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn crashes_and_recoveries_are_reported() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), Capabilities::default(), tx);
        let (env, dep) = (Id::new(), Id::new());
        let ds = state(1, env, vec![workload(dep, env, "")]);
        r.apply(&ds).await.unwrap();
        settle(&r, dep).await;
        phases(&mut rx, dep);

        rt.crash(&dep.to_string(), 137);
        r.apply(&ds).await.unwrap();
        assert_eq!(phases(&mut rx, dep), [Phase::Crashed]);
        rt.recover(&dep.to_string());
        r.apply(&ds).await.unwrap();
        assert_eq!(phases(&mut rx, dep), [Phase::Active]);
        assert_eq!(
            r.replay().len(),
            1,
            "current phase is replayed after reconnect"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn routes_follow_the_serving_deployment() {
        let rt = Arc::new(FakeRuntime::new());
        let edge = Arc::new(crate::edge::RecordingEdge::default());
        let (tx, _rx) = mpsc::channel(256);
        let r = Reconciler::new(
            rt.clone(),
            Capabilities {
                edge: Some(edge.clone()),
                ..Default::default()
            },
            tx,
        );
        let (env, dep) = (Id::new(), Id::new());
        let mut ds = state(1, env, vec![workload(dep, env, "")]);
        r.apply(&ds).await.unwrap();
        settle(&r, dep).await;
        ds.generation = 2;
        ds.routes = vec![pb::Route {
            hostname: "API.apps.test".into(),
            deployment_id: dep.to_string(),
            port: 8080,
            proxied: true,
            rules: vec![pb::EdgeRule {
                any: vec![],
                action: Some(pb::edge_rule::Action::Allow(pb::EdgeAllow {})),
            }],
            ..Default::default()
        }];
        ds.edge = Some(pb::EdgeSettings {
            tls: pb::TlsMode::TlsInternal as i32,
            acme_email: String::new(),
            dashboard_hosts: vec![],
        });
        r.apply(&ds).await.unwrap();
        let cfg = edge.last.lock().unwrap().clone().unwrap();
        assert_eq!(cfg.tls, pb::TlsMode::TlsInternal);
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].upstream.port(), 8080);
        // Rules and Cloudflare proxying come along, by normalized host.
        let site = &cfg.sites["api.apps.test"];
        assert_eq!(site.deployment_id, dep.to_string());
        assert!(site.proxied);
        assert_eq!(site.rules.len(), 1);
        // A crashed container is not routed to.
        rt.crash(&dep.to_string(), 1);
        r.apply(&ds).await.unwrap();
        assert!(
            edge.last
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .routes
                .is_empty()
        );
    }

    /// A loopback "app" answering every request with 200 and its path.
    async fn http_app() -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = l.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 4096];
                    let Ok(n) = c.read(&mut buf).await else {
                        return;
                    };
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{path}",
                        path.len()
                    );
                    let _ = c.write_all(resp.as_bytes()).await;
                });
            }
        });
        port
    }

    /// A request as Caddy sends it to the wake proxy for a sleeping site.
    async fn through_proxy(waker: &Waker, header: &str, path: &str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut c = tokio::net::TcpStream::connect(waker.addr()).await.unwrap();
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: api.apps.test\r\n{}: {header}\r\nConnection: close\r\n\r\n",
            crate::edge::wake::HEADER
        );
        c.write_all(req.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        out
    }

    /// Every event so far for `dep`, as (phase, cold start ms).
    fn events_of(rx: &mut mpsc::Receiver<pb::NodeMessage>, dep: Id) -> Vec<(Phase, u32)> {
        let mut out = vec![];
        while let Ok(m) = rx.try_recv() {
            if let Some(Msg::Event(e)) = m.msg
                && e.deployment_id == dep.to_string()
            {
                out.push((Phase::try_from(e.phase).unwrap(), e.cold_start_ms));
            }
        }
        out
    }

    struct Serverless {
        rt: Arc<FakeRuntime>,
        edge: Arc<crate::edge::RecordingEdge>,
        waker: Arc<Waker>,
        r: Arc<Reconciler>,
        rx: mpsc::Receiver<pb::NodeMessage>,
        ds: pb::DesiredState,
        dep: Id,
        port: u16,
        _stop: CancellationToken,
    }

    /// A serverless deployment (1 s idle) with a route, rolled out and ACTIVE.
    async fn serverless(health: &str) -> Serverless {
        let rt = Arc::new(FakeRuntime::new());
        *rt.address_override.lock().unwrap() = Some(std::net::IpAddr::from([127, 0, 0, 1]));
        let edge = Arc::new(crate::edge::RecordingEdge::default());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let waker = Waker::new(l.local_addr().unwrap());
        let stop = CancellationToken::new();
        tokio::spawn(waker.clone().serve(l, stop.clone()));
        let (tx, rx) = mpsc::channel(256);
        let r = Reconciler::new(
            rt.clone(),
            Capabilities {
                edge: Some(edge.clone()),
                wake: Some(waker.clone()),
                ..Default::default()
            },
            tx,
        );
        let port = http_app().await;
        let (env, dep) = (Id::new(), Id::new());
        let mut w = workload(dep, env, health);
        w.port = u32::from(port);
        w.serverless = Some(pb::Serverless { idle_secs: 1 });
        let mut ds = state(1, env, vec![w]);
        ds.routes = vec![pb::Route {
            hostname: "api.apps.test".into(),
            deployment_id: dep.to_string(),
            port: u32::from(port),
            ..Default::default()
        }];
        r.apply(&ds).await.unwrap();
        for _ in 0..100 {
            if !r.rollout_running(&dep.to_string()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut s = Serverless {
            rt,
            edge,
            waker,
            r,
            rx,
            ds,
            dep,
            port,
            _stop: stop,
        };
        assert_eq!(events_of(&mut s.rx, dep).last(), Some(&(Phase::Active, 0)));
        s
    }

    impl Serverless {
        fn edge_cfg(&self) -> EdgeConfig {
            self.edge.last.lock().unwrap().clone().unwrap()
        }

        /// Idle long enough, then one reconcile: asleep, routed to the proxy.
        async fn sleep(&mut self) {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            self.r.apply(&self.ds).await.unwrap();
            assert_eq!(events_of(&mut self.rx, self.dep), [(Phase::Sleeping, 0)]);
            assert!(self.rt.running_deployments().is_empty(), "stopped");
            assert_eq!(
                self.waker.state(&self.dep.to_string()),
                Some(WakeState::Sleeping)
            );
        }
    }

    #[tokio::test]
    async fn an_idle_serverless_deployment_sleeps_and_a_request_wakes_it() {
        let mut s = serverless("").await;
        let dep = s.dep.to_string();
        // Requests in flight at the edge keep it awake.
        let addr: std::net::SocketAddr = format!("127.0.0.1:{}", s.port).parse().unwrap();
        s.edge.in_flight.lock().unwrap().insert(addr, 1);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        s.r.apply(&s.ds).await.unwrap();
        assert!(events_of(&mut s.rx, s.dep).is_empty());
        assert_eq!(s.rt.running_deployments(), [dep.clone()].into());
        s.edge.in_flight.lock().unwrap().clear();

        s.sleep().await;
        let (starts, stops) = s.rt.start_stops();
        assert_eq!(stops, 1);
        // The route now dials the wake proxy, naming the deployment.
        let cfg = s.edge_cfg();
        assert_eq!(cfg.routes[0].upstream, s.waker.addr());
        let header = cfg.sites["api.apps.test"].wake.clone().unwrap();
        assert!(header.starts_with(&format!("{dep} {} ", s.port)));
        // A stopped container while asleep is expected: no crash.
        s.r.apply(&s.ds).await.unwrap();
        assert!(events_of(&mut s.rx, s.dep).is_empty());
        assert!(
            s.r.replay().iter().any(
                |m| matches!(&m.msg, Some(Msg::Event(e)) if e.phase == Phase::Sleeping as i32)
            )
        );

        // The held request is served by the woken container.
        let out = through_proxy(&s.waker, &header, "/hello").await;
        assert!(
            out.starts_with("HTTP/1.1 200") && out.ends_with("/hello"),
            "{out}"
        );
        assert_eq!(s.rt.start_stops().0, starts + 1);
        // ACTIVE again with the cold start, then routed to the container.
        s.r.apply(&s.ds).await.unwrap();
        let evs = events_of(&mut s.rx, s.dep);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].0, Phase::Active);
        assert!(evs[0].1 > 0, "cold start recorded");
        s.r.apply(&s.ds).await.unwrap();
        let cfg = s.edge_cfg();
        assert_eq!(cfg.routes[0].upstream.port(), s.port);
        assert_eq!(cfg.sites["api.apps.test"].wake, None);
        // And it sleeps again when idle.
        s.sleep().await;
    }

    #[tokio::test]
    async fn a_wake_whose_health_check_never_passes_answers_504() {
        let mut s = serverless("/health").await;
        s.sleep().await;
        // Health checks fail from now on; the wake gives up after the
        // check's timeout (2 s) and the container sleeps again.
        s.rt.unhealthy.lock().unwrap().insert(s.dep.to_string());
        let header = s.edge_cfg().sites["api.apps.test"].wake.clone().unwrap();
        let out = through_proxy(&s.waker, &header, "/").await;
        assert!(out.starts_with("HTTP/1.1 504"), "{out}");
        assert!(s.rt.running_deployments().is_empty());
        s.r.apply(&s.ds).await.unwrap();
        assert!(events_of(&mut s.rx, s.dep).is_empty(), "still asleep");
        // Healthy again: the next request wakes it.
        s.rt.unhealthy.lock().unwrap().clear();
        assert!(
            through_proxy(&s.waker, &header, "/ok")
                .await
                .ends_with("/ok")
        );
    }

    /// After a node restart, a serverless deployment's stopped container is
    /// asleep (routed to the proxy), not crashed.
    #[tokio::test(start_paused = true)]
    async fn a_stopped_serverless_container_is_adopted_asleep() {
        let rt = Arc::new(FakeRuntime::new());
        let (env, dep, plain) = (Id::new(), Id::new(), Id::new());
        let mut w = workload(dep, env, "");
        w.serverless = Some(pb::Serverless { idle_secs: 600 });
        let ds = state(1, env, vec![w, workload(plain, env, "")]);
        {
            let (tx, _rx) = mpsc::channel(256);
            let first = Reconciler::new(rt.clone(), Capabilities::default(), tx);
            first.apply(&ds).await.unwrap();
            settle(&first, dep).await;
            settle(&first, plain).await;
        }
        for c in rt.containers().await.unwrap() {
            rt.stop(&c.id).await.unwrap();
        }
        let edge = Arc::new(crate::edge::RecordingEdge::default());
        let waker = Waker::new("127.0.0.1:7104".parse().unwrap());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(
            rt.clone(),
            Capabilities {
                edge: Some(edge),
                wake: Some(waker.clone()),
                ..Default::default()
            },
            tx,
        );
        r.apply(&ds).await.unwrap();
        let events = drain(&mut rx);
        assert_eq!(of(&events, dep), [Phase::Sleeping]);
        assert_eq!(of(&events, plain), [Phase::Crashed]);
        assert_eq!(waker.state(&dep.to_string()), Some(WakeState::Sleeping));
        // Gone from the desired state: forgotten.
        r.apply(&state(2, env, vec![])).await.unwrap();
        assert_eq!(waker.state(&dep.to_string()), None);
    }

    #[test]
    fn idle_times_read_well() {
        assert_eq!(idle_text(600), "10 min");
        assert_eq!(idle_text(7200), "2 h");
        assert_eq!(idle_text(45), "45 s");
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_workloads_are_ignored() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, _rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), Capabilities::default(), tx);
        let env = Id::new();
        let mut w = workload(Id::new(), env, "");
        w.service_name = "web; rm -rf /".into();
        r.apply(&state(1, env, vec![w])).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(rt.running_deployments().is_empty());
    }

    fn with_builder() -> (Capabilities, std::path::PathBuf) {
        with_overrides(Default::default())
    }

    fn with_overrides(
        overrides: crate::build::RuntimeOverrides,
    ) -> (Capabilities, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("modula-reconcile-{}", Id::new()));
        let caps = Capabilities {
            build: Some(BuildSupport {
                builder: Arc::new(crate::build::FakeBuilder {
                    overrides,
                    ..Default::default()
                }),
                logs: crate::build::BuildLogs::new(&dir).unwrap(),
            }),
            ..Default::default()
        };
        (caps, dir)
    }

    fn built(dep: Id, env: Id, source: &str) -> pb::Workload {
        pb::Workload {
            image: format!("{LOCAL_PREFIX}svc:{dep}"),
            build: Some(pb::Build {
                source_url: source.into(),
                ..Default::default()
            }),
            ..workload(dep, env, "")
        }
    }

    /// Functions run in the node's workerd, not containers: inline code
    /// right away, a repository's bundle after its build; their routes go
    /// to their sockets, and they leave workerd with the desired state.
    #[tokio::test]
    async fn functions_run_in_workerd_and_are_routed_there() {
        let rt = Arc::new(FakeRuntime::new());
        let edge = Arc::new(crate::edge::RecordingEdge::default());
        let (mut caps, dir) = with_builder();
        let fake = Arc::new(crate::functions::FakeWorkerd::default());
        let fns = crate::functions::Functions::new(dir.join("functions"), fake.clone()).unwrap();
        let stop = CancellationToken::new();
        let (out, _shipped) = mpsc::channel(64);
        tokio::spawn(fns.clone().run(out, stop.clone()));
        caps.edge = Some(edge.clone());
        caps.functions = Some(fns.clone());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), caps, tx);
        let (env, inline, repo, broken) = (Id::new(), Id::new(), Id::new(), Id::new());
        let function = |code: &str| pb::Function {
            code: code.into(),
            compatibility_date: "2026-10-01".into(),
        };
        let w_inline = pb::Workload {
            function: Some(function("export default {}")),
            ..workload(inline, env, "")
        };
        let w_repo = pb::Workload {
            function: Some(function("")),
            build: Some(pb::Build {
                source_url: "https://example.com/src.tar.gz".into(),
                builder: pb::Builder::Function as i32,
                function_entry: "index.ts".into(),
                ..Default::default()
            }),
            ..workload(repo, env, "/")
        };
        let w_broken = pb::Workload {
            function: Some(function(crate::functions::process::FAIL_MARK)),
            ..workload(broken, env, "")
        };
        let mut ds = state(1, env, vec![w_inline.clone(), w_repo.clone(), w_broken]);
        r.apply(&ds).await.unwrap();
        for d in [inline, repo, broken] {
            settle(&r, d).await;
        }
        let events = drain(&mut rx);
        assert_eq!(of(&events, inline), [Phase::Starting, Phase::Active]);
        assert_eq!(
            of(&events, repo),
            [
                Phase::Building,
                Phase::Built,
                Phase::Starting,
                Phase::Healthchecking,
                Phase::Active
            ]
        );
        assert_eq!(of(&events, broken), [Phase::Starting, Phase::Failed]);
        assert!(rt.containers().await.unwrap().is_empty(), "no containers");
        assert_eq!(
            fns.saved_code(&repo.to_string()).as_deref(),
            Some(crate::build::FAKE_FUNCTION),
            "the bundle is kept for node restarts"
        );
        assert!(!fns.contains(&broken.to_string()));

        // Seen again: nothing new rolls out.
        let launches = fake.launches().len();
        r.apply(&ds).await.unwrap();
        assert!(drain(&mut rx).is_empty());
        assert_eq!(fake.launches().len(), launches);

        ds.generation = 2;
        ds.workloads.truncate(2);
        ds.routes = vec![pb::Route {
            hostname: "fn.apps.test".into(),
            deployment_id: inline.to_string(),
            port: 8080,
            ..Default::default()
        }];
        r.apply(&ds).await.unwrap();
        let cfg = edge.last.lock().unwrap().clone().unwrap();
        let port = fns.port(&inline.to_string()).unwrap();
        assert_eq!(
            cfg.routes,
            [ResolvedRoute {
                hostname: "fn.apps.test".into(),
                upstream: std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            }]
        );

        // Out of the desired state: out of workerd.
        ds.generation = 3;
        ds.workloads = vec![w_repo];
        ds.routes.clear();
        r.apply(&ds).await.unwrap();
        assert!(!fns.contains(&inline.to_string()));
        assert!(fns.contains(&repo.to_string()));
        stop.cancel();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn builds_then_runs_the_local_image() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let (caps, dir) = with_builder();
        let logs = caps.build.as_ref().unwrap().logs.clone();
        let r = Reconciler::new(rt.clone(), caps, tx);
        let (env, ok, bad) = (Id::new(), Id::new(), Id::new());
        // A local image must never be pulled: make any pull of it fail.
        rt.unpullable
            .lock()
            .unwrap()
            .insert(format!("{LOCAL_PREFIX}svc:{ok}"));
        r.apply(&state(
            1,
            env,
            vec![
                built(ok, env, "https://x/ok"),
                built(bad, env, "https://x/fail"),
            ],
        ))
        .await
        .unwrap();
        settle(&r, ok).await;
        settle(&r, bad).await;
        let events = drain(&mut rx);
        assert_eq!(
            of(&events, ok),
            [
                Phase::Building,
                Phase::Built,
                Phase::Creating,
                Phase::Starting,
                Phase::Active
            ]
        );
        assert_eq!(of(&events, bad), [Phase::Building, Phase::Failed]);
        let mut log = logs.read(&bad.to_string(), 100, false).unwrap();
        let lines = log.next_batch().await.unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.stderr && l.text.contains("the build failed"))
        );

        // Without a builder, a build is a clear failure, not a pull attempt.
        let (tx, mut rx) = mpsc::channel(64);
        let plain = Reconciler::new(rt.clone(), Capabilities::default(), tx);
        let other = Id::new();
        plain
            .apply(&state(1, env, vec![built(other, env, "https://x/ok")]))
            .await
            .unwrap();
        settle(&plain, other).await;
        assert_eq!(phases(&mut rx, other), [Phase::Failed]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn repo_config_overrides_reach_the_rollout() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let (caps, dir) = with_overrides(crate::build::RuntimeOverrides {
            healthcheck_path: Some("/up".into()),
            ..Default::default()
        });
        let r = Reconciler::new(rt.clone(), caps, tx);
        let (env, dep) = (Id::new(), Id::new());
        rt.unhealthy.lock().unwrap().insert(dep.to_string());
        // The dashboard has no health check; the file's applies.
        r.apply(&state(1, env, vec![built(dep, env, "https://x/ok")]))
            .await
            .unwrap();
        settle(&r, dep).await;
        assert_eq!(
            phases(&mut rx, dep),
            [
                Phase::Building,
                Phase::Built,
                Phase::Creating,
                Phase::Starting,
                Phase::Healthchecking,
                Phase::Failed
            ]
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn pre_deploy_runs_before_start_and_its_failure_keeps_the_old_one() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), Capabilities::default(), tx);
        let (env, d1, d2) = (Id::new(), Id::new(), Id::new());
        r.apply(&state(1, env, vec![workload(d1, env, "")]))
            .await
            .unwrap();
        settle(&r, d1).await;
        drain(&mut rx);

        // The new deployment's migration fails: it never starts, d1 keeps running.
        let migrate = vec!["./migrate".to_owned(), "up".into()];
        rt.once_exit
            .lock()
            .unwrap()
            .insert("./migrate up".into(), 3);
        let mut w2 = workload(d2, env, "");
        w2.pre_deploy_command = migrate.clone();
        // A credential-looking variable whose value shows up in the error.
        w2.env = vec![pb::EnvVar {
            name: "MIGRATE_TOKEN".into(),
            value: "./migrate".into(),
        }];
        r.apply(&state(2, env, vec![workload(d1, env, ""), w2.clone()]))
            .await
            .unwrap();
        settle(&r, d2).await;
        assert_eq!(
            phases(&mut rx, d2),
            [Phase::Pulling, Phase::Creating, Phase::Failed]
        );
        let message = r.tracked.lock().unwrap()[&d2.to_string()].message.clone();
        assert!(
            message.starts_with("Pre-deploy command exited with code 3"),
            "{message}"
        );
        assert!(
            !message.contains("ran ./migrate"),
            "variable values are masked in the error: {message}"
        );
        assert_eq!(rt.running_deployments(), [d1.to_string()].into());
        let once = rt.onces.lock().unwrap()[0].clone();
        assert_eq!(once.cmd, migrate);
        assert_eq!(once.network, env.to_string());
        assert_eq!(once.running, [d1.to_string()].into());

        // Exit 0: the deployment goes on to start.
        let d3 = Id::new();
        rt.once_exit.lock().unwrap().clear();
        let w3 = pb::Workload {
            deployment_id: d3.to_string(),
            ..w2
        };
        r.apply(&state(3, env, vec![workload(d1, env, ""), w3]))
            .await
            .unwrap();
        settle(&r, d3).await;
        assert_eq!(phases(&mut rx, d3).last(), Some(&Phase::Active));
        assert!(rt.running_deployments().contains(&d3.to_string()));
        assert_eq!(rt.onces.lock().unwrap().len(), 2);
    }
}
