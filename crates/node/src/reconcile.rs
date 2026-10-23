//! Converges the runtime to the newest desired state.
//!
//! Each new workload gets a rollout task: pull → create → start → health
//! check → ACTIVE (or FAILED, after which its container is removed). A periodic
//! pass repairs drift and reports crashes and recoveries. Containers whose
//! deployment is no longer desired are stopped and removed; nothing without
//! our labels is ever touched.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use modula_proto::node::v1 as pb;
use pb::DeploymentPhase as Phase;
use pb::node_message::Msg;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::edge::{EdgeConfig, EdgeSink, ResolvedRoute};
use crate::runtime::{Container, Runtime};

const REPAIR_EVERY: Duration = Duration::from_secs(2);
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// Without an HTTP health check, a container must stay up this long.
const SETTLE: Duration = Duration::from_secs(2);

struct Tracked {
    phase: Phase,
    message: String,
    rollout: Option<JoinHandle<()>>,
}

pub struct Reconciler {
    runtime: Arc<dyn Runtime>,
    edge: Option<Arc<dyn EdgeSink>>,
    out: mpsc::Sender<pb::NodeMessage>,
    tracked: Mutex<HashMap<String, Tracked>>,
    applied: AtomicU64,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// The server validates these too; the node does not trust them blindly
/// because they end up in container names and labels.
fn workload_is_sane(w: &pb::Workload) -> bool {
    let name_ok = !w.service_name.is_empty()
        && w.service_name.len() <= 63
        && w.service_name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    name_ok
        && Uuid::parse_str(&w.deployment_id).is_ok()
        && Uuid::parse_str(&w.environment_id).is_ok()
        && u16::try_from(w.port).is_ok_and(|p| p > 0)
}

impl Reconciler {
    pub fn new(
        runtime: Arc<dyn Runtime>,
        edge: Option<Arc<dyn EdgeSink>>,
        out: mpsc::Sender<pb::NodeMessage>,
    ) -> Arc<Self> {
        Arc::new(Self {
            runtime,
            edge,
            out,
            tracked: Mutex::new(HashMap::new()),
            applied: AtomicU64::new(0),
        })
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
            .filter(|(_, t)| matches!(t.phase, Phase::Active | Phase::Failed | Phase::Crashed))
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
            tokio::select! {
                _ = stop.cancelled() => break,
                changed = desired.changed() => if changed.is_err() { break },
                _ = tick.tick() => {}
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
        let message = message.into();
        {
            let mut tracked = self.tracked.lock().expect("tracked lock");
            let t = tracked.entry(deployment.to_owned()).or_insert(Tracked {
                phase: Phase::PhaseUnspecified,
                message: String::new(),
                rollout: None,
            });
            t.phase = phase;
            t.message = message.clone();
            if !matches!(
                phase,
                Phase::Pulling | Phase::Creating | Phase::Starting | Phase::Healthchecking
            ) {
                t.rollout = None;
            }
        }
        tracing::info!(deployment, phase = ?phase, %message, "deployment phase");
        self.emit(event(
            deployment,
            phase,
            &message,
            self.applied_generation(),
        ));
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

        // 1. Point the edge at what serves now, *before* retiring anything, so
        //    traffic moves to a new deployment before the old one stops.
        let containers = self.runtime.containers().await?;
        self.apply_edge(ds, &containers).await;

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

        // 3. Start, adopt, and watch what is wanted.
        let containers = self.runtime.containers().await?;
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
        };
        self.emit(pb::NodeMessage {
            msg: Some(Msg::Report(report)),
        });
        Ok(())
    }

    /// Resolve routes to running containers and hand them to the edge. Edge
    /// failures are logged, never fatal: workloads keep reconciling.
    async fn apply_edge(&self, ds: &pb::DesiredState, containers: &[Container]) {
        let Some(edge) = &self.edge else { return };
        let mut routes = Vec::new();
        for r in &ds.routes {
            let Ok(port) = u16::try_from(r.port) else {
                continue;
            };
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
        }
        let settings = ds.edge.clone().unwrap_or_default();
        let cfg = EdgeConfig {
            routes,
            tls: pb::TlsMode::try_from(settings.tls).unwrap_or(pb::TlsMode::TlsAcme),
            acme_email: settings.acme_email,
        };
        if let Err(e) = edge.apply(&cfg).await {
            tracing::warn!(error = format!("{e:#}"), "edge update failed; will retry");
        }
    }

    fn rollout_running(&self, deployment: &str) -> bool {
        let tracked = self.tracked.lock().expect("tracked lock");
        tracked
            .get(deployment)
            .and_then(|t| t.rollout.as_ref())
            .is_some_and(|h| !h.is_finished())
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
        let phase = self.phase_of(id);
        if phase == Some(Phase::Failed) {
            return Ok(()); // the server removes it from the desired state
        }
        // Extra containers for one deployment can only come from a crash
        // mid-rollout; keep the first.
        for extra in containers.iter().skip(1) {
            self.runtime.remove(&extra.id).await?;
        }
        match (containers.first(), phase) {
            (None, _) => self.start_rollout(w.clone()),
            // Adopt a container that survived a node restart.
            (Some(c), None) if c.running => self.set_phase(id, Phase::Active, "running"),
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
        let t = tracked.entry(id).or_insert(Tracked {
            phase: Phase::PhaseUnspecified,
            message: String::new(),
            rollout: None,
        });
        t.rollout = Some(handle);
    }

    async fn rollout(&self, w: &pb::Workload) -> Result<(), String> {
        let id = w.deployment_id.as_str();
        self.set_phase(id, Phase::Pulling, format!("Pulling {}", w.image));
        self.runtime
            .pull(&w.image)
            .await
            .map_err(|e| format!("Image pull failed: {e:#}"))?;

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
            self.ensure_running(&container).await?;
        } else {
            self.set_phase(
                id,
                Phase::Healthchecking,
                format!("Waiting for {} to return 2xx", health.path),
            );
            let deadline = tokio::time::Instant::now()
                + Duration::from_secs(u64::from(health.timeout_secs.max(1)));
            loop {
                self.ensure_running(&container).await?;
                if self
                    .runtime
                    .probe(&container, &w.environment_id, port, &health.path)
                    .await
                    .unwrap_or(false)
                {
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
        self.set_phase(id, Phase::Active, "Deployment is live");
        Ok(())
    }

    async fn ensure_running(&self, container: &str) -> Result<(), String> {
        let cs = self
            .runtime
            .containers()
            .await
            .map_err(|e| format!("{e:#}"))?;
        match cs.iter().find(|c| c.id == container) {
            Some(c) if c.running => Ok(()),
            Some(c) => Err(format!(
                "The container exited during startup (code {})",
                c.exit_code
            )),
            None => Err("The container disappeared during startup".into()),
        }
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
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::fake::FakeRuntime;

    fn workload(dep: Uuid, env: Uuid, health: &str) -> pb::Workload {
        pb::Workload {
            deployment_id: dep.to_string(),
            service_id: Uuid::nil().to_string(),
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

    fn state(generation: u64, env: Uuid, ws: Vec<pb::Workload>) -> pb::DesiredState {
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

    fn of(events: &[(String, Phase)], dep: Uuid) -> Vec<Phase> {
        events
            .iter()
            .filter(|(d, _)| *d == dep.to_string())
            .map(|(_, p)| *p)
            .collect()
    }

    /// Drain and return the phases emitted for `dep`.
    fn phases(rx: &mut mpsc::Receiver<pb::NodeMessage>, dep: Uuid) -> Vec<Phase> {
        of(&drain(rx), dep)
    }

    async fn settle(r: &Arc<Reconciler>, dep: Uuid) {
        for _ in 0..50 {
            if !r.rollout_running(&dep.to_string()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("rollout did not finish");
    }

    #[tokio::test(start_paused = true)]
    async fn rolls_out_supersedes_and_removes() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, mut rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), None, tx);
        let (env, d1, d2) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());

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
        let r = Reconciler::new(rt.clone(), None, tx);
        let (env, bad_pull, unhealthy) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
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
        let r = Reconciler::new(rt.clone(), None, tx);
        let (env, dep) = (Uuid::now_v7(), Uuid::now_v7());
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
        let r = Reconciler::new(rt.clone(), Some(edge.clone()), tx);
        let (env, dep) = (Uuid::now_v7(), Uuid::now_v7());
        let mut ds = state(1, env, vec![workload(dep, env, "")]);
        r.apply(&ds).await.unwrap();
        settle(&r, dep).await;
        ds.generation = 2;
        ds.routes = vec![pb::Route {
            hostname: "api.apps.test".into(),
            deployment_id: dep.to_string(),
            port: 8080,
        }];
        ds.edge = Some(pb::EdgeSettings {
            tls: pb::TlsMode::TlsInternal as i32,
            acme_email: String::new(),
        });
        r.apply(&ds).await.unwrap();
        let cfg = edge.last.lock().unwrap().clone().unwrap();
        assert_eq!(cfg.tls, pb::TlsMode::TlsInternal);
        assert_eq!(cfg.routes.len(), 1);
        assert_eq!(cfg.routes[0].upstream.port(), 8080);
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

    #[tokio::test(start_paused = true)]
    async fn malformed_workloads_are_ignored() {
        let rt = Arc::new(FakeRuntime::new());
        let (tx, _rx) = mpsc::channel(256);
        let r = Reconciler::new(rt.clone(), None, tx);
        let env = Uuid::now_v7();
        let mut w = workload(Uuid::now_v7(), env, "");
        w.service_name = "web; rm -rf /".into();
        r.apply(&state(1, env, vec![w])).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(rt.running_deployments().is_empty());
    }
}
