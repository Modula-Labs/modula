//! Converges the runtime to the newest desired state.
//!
//! Each new workload gets a rollout task: [build →] pull → create → start →
//! health check → ACTIVE (or FAILED, after which its container is removed). A periodic
//! pass repairs drift and reports crashes and recoveries. Containers whose
//! deployment is no longer desired are stopped and removed; nothing without
//! our labels is ever touched.

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
use crate::edge::{EdgeConfig, EdgeSink, ResolvedRoute};
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
}

/// Optional parts of a node: an edge (Caddy), a builder and database access.
#[derive(Clone, Default)]
pub struct Capabilities {
    pub edge: Option<Arc<dyn EdgeSink>>,
    pub build: Option<BuildSupport>,
    /// The Database tab ([`crate::db`]).
    pub db: Option<Arc<dyn crate::db::Executor>>,
}

pub struct Reconciler {
    runtime: Arc<dyn Runtime>,
    edge: Option<Arc<dyn EdgeSink>>,
    build: Option<BuildSupport>,
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
fn workload_is_sane(w: &pb::Workload) -> bool {
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
        Arc::new(Self {
            runtime,
            edge: caps.edge,
            build: caps.build,
            out,
            tracked: Mutex::new(HashMap::new()),
            applied: AtomicU64::new(0),
            tcp: TcpProxies::default(),
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
        self.set_phase_with(deployment, phase, message, None);
    }

    fn set_phase_with(
        &self,
        deployment: &str,
        phase: Phase,
        message: impl Into<String>,
        build: Option<pb::BuildOutput>,
    ) {
        let message = message.into();
        {
            let mut tracked = self.tracked.lock().expect("tracked lock");
            let t = tracked.entry(deployment.to_owned()).or_insert(Tracked {
                phase: Phase::PhaseUnspecified,
                message: String::new(),
                rollout: None,
                restarted: None,
            });
            t.phase = phase;
            t.message = message.clone();
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

        // 1. Point the edge at what serves now, *before* retiring anything, so
        //    traffic moves to a new deployment before the old one stops.
        let containers = self.runtime.containers().await?;
        self.apply_edge(ds, &containers).await;
        self.apply_tcp(ds, &containers).await;

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
        let phase = self.phase_of(id);
        if phase == Some(Phase::Failed) {
            return Ok(()); // the server removes it from the desired state
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
            restarted: None,
        });
        t.rollout = Some(handle);
    }

    async fn rollout(&self, w: &pb::Workload) -> Result<(), String> {
        let id = w.deployment_id.as_str();
        let mut effective;
        let w = match &w.build {
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
        let result = self
            .runtime
            .run_once(w, &w.pre_deploy_command, PRE_DEPLOY_TIMEOUT, sink)
            .await;
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

/// Masks the workload's variable values in text that leaves the node outside
/// the log stream (which the server redacts itself), e.g. a failure message.
fn redact(text: &str, env: &[pb::EnvVar]) -> String {
    let mut out = text.to_owned();
    let mut secrets: Vec<&str> = env
        .iter()
        .filter(|e| !e.name.starts_with("MODULA_") && e.name != "PORT" && e.value.len() >= 6)
        .map(|e| e.value.as_str())
        .collect();
    // Longest first, so a value containing another is masked whole.
    secrets.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for v in secrets {
        out = out.replace(v, "••••••");
    }
    out
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
                builder: Arc::new(crate::build::FakeBuilder { overrides }),
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
        w2.env = vec![pb::EnvVar {
            name: "DATABASE_URL".into(),
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
