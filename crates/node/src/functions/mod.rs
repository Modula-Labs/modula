//! JavaScript functions, run by workerd (the open-source Workers runtime)
//! instead of containers.
//!
//! One workerd process serves every function on this node, each as its own
//! service (its own V8 isolate) behind its own loopback socket, which Caddy
//! routes the function's domains to. Functions are always warm. The process
//! is supervised: whenever the set of functions changes its config is
//! rendered anew ([`capnp`]) and it is restarted (in-flight requests get a
//! grace period; new ones see a brief gap), and when it dies it is started
//! again with backoff. A function joins only after it started alone in a
//! throwaway workerd: one that fails to load would take the others down.
//!
//! Sandbox: workerd runs as its own unprivileged user ([`process::USER`])
//! with a cleared environment, inside bubblewrap where that works; its
//! network config lets functions reach the public internet only (not this
//! server, its networks, the private network between servers or the
//! metadata service), and the host firewall refuses the same for its user.
//! The config holds the functions' variables, so it is that user's alone.
//!
//! Logs: each function's console is tagged with its deployment (see
//! [`capnp::console_module`]); the supervisor ships its lines like
//! container output and keeps the latest for the deployment's log tab.

pub mod capnp;
pub mod process;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use modula_proto::node::v1 as pb;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::runtime::LogLine;
pub use process::{FakeWorkerd, Launcher, Workerd};

/// Loopback ports functions listen on, one each.
const PORTS: std::ops::RangeInclusive<u16> = 47100..=47999;
/// How long workerd may take to load its functions and listen.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// Changes arriving together cause one restart.
const DEBOUNCE: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const SHIP_EVERY: Duration = Duration::from_secs(2);
/// Lines kept per function for its log tab.
const RECENT: usize = 1000;
/// Lines held for shipping while the server can't take them.
const SHIP_BUFFER: usize = 50_000;
const MAX_LINE: usize = 8 * 1024;
/// An untagged line this soon after a function's line on the same stream
/// continues it (a multi-line message); later ones are workerd's own.
const CONTINUES_WITHIN: Duration = Duration::from_millis(250);
/// The tag the console module prefixes: `\x1e<deployment>\x1f `.
const TAG_START: char = '\u{1e}';
const TAG_END: char = '\u{1f}';
const CONFIG: &str = "workerd.capnp";

/// One function, as workerd runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSpec {
    pub deployment_id: String,
    pub compatibility_date: String,
    /// The module worker.
    pub code: String,
    /// Text bindings: the deployment's variables.
    pub bindings: Vec<(String, String)>,
}

impl FunctionSpec {
    /// A workload's function with `code`; variables that can't be bindings
    /// (not identifiers, or holding NUL) are left out.
    pub fn new(w: &pb::Workload, f: &pb::Function, code: String) -> Self {
        let mut bindings: Vec<(String, String)> = w
            .env
            .iter()
            .filter(|e| binding_name_ok(&e.name) && !e.value.contains('\0'))
            .map(|e| (e.name.clone(), e.value.clone()))
            .collect();
        bindings.sort();
        bindings.dedup_by(|a, b| a.0 == b.0);
        Self {
            deployment_id: w.deployment_id.clone(),
            compatibility_date: f.compatibility_date.clone(),
            code,
            bindings,
        }
    }
}

fn binding_name_ok(n: &str) -> bool {
    let mut b = n.bytes();
    b.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        && n.len() <= 128
}

/// A deployment id as a directory and service name: ids only.
fn safe_id(d: &str) -> bool {
    d.parse::<modula_id::Id>().is_ok()
}

struct Entry {
    spec: FunctionSpec,
    port: u16,
}

#[derive(Default)]
struct State {
    entries: BTreeMap<String, Entry>,
    /// Bumped on every change; the process serves one version at a time.
    version: u64,
}

/// What the process serves right now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Serving {
    version: u64,
    up: bool,
}

pub struct Functions {
    dir: PathBuf,
    launcher: Arc<dyn Launcher>,
    state: Mutex<State>,
    changed: watch::Sender<u64>,
    serving: watch::Sender<Serving>,
    /// Since when workerd hasn't been serving, while it isn't.
    down_since: Mutex<Option<Instant>>,
    logs: Logs,
}

impl Functions {
    /// Functions kept in `dir` (created, and handed to workerd's user).
    pub fn new(dir: PathBuf, launcher: Arc<dyn Launcher>) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            // Its user must reach the directory, not list what is beside it.
            let mut cur = dir.parent().filter(|_| launcher.own_user());
            while let Some(p) = cur.filter(|p| !p.as_os_str().is_empty() && *p != Path::new("/")) {
                crate::build::buildkit::traversable(p)?;
                cur = p.parent();
            }
        }
        launcher.own(&dir)?;
        Ok(Arc::new(Self {
            dir,
            launcher,
            state: Mutex::new(State::default()),
            changed: watch::Sender::new(0),
            serving: watch::Sender::new(Serving::default()),
            down_since: Mutex::new(None),
            logs: Logs::default(),
        }))
    }

    /// The port `deployment` listens on, once it is in the process.
    pub fn port(&self, deployment: &str) -> Option<u16> {
        let s = self.state.lock().expect("functions lock");
        s.entries.get(deployment).map(|e| e.port)
    }

    pub fn contains(&self, deployment: &str) -> bool {
        self.port(deployment).is_some()
    }

    fn set_serving(&self, s: Serving) {
        let mut since = self.down_since.lock().expect("functions lock");
        match (s.up, *since) {
            (true, _) => *since = None,
            (false, None) => *since = Some(Instant::now()),
            (false, Some(_)) => {}
        }
        self.serving.send_replace(s);
    }

    /// How long workerd has been down (zero while it runs).
    pub fn down_for(&self) -> Duration {
        self.down_since
            .lock()
            .expect("functions lock")
            .map_or(Duration::ZERO, |t| t.elapsed())
    }

    /// Whether workerd is running and serves the latest functions.
    pub fn up(&self) -> bool {
        let version = self.state.lock().expect("functions lock").version;
        let s = *self.serving.borrow();
        s.up && s.version == version
    }

    /// The code a build made for `deployment` earlier (kept across node
    /// restarts, so a function isn't built again).
    pub fn saved_code(&self, deployment: &str) -> Option<String> {
        if !safe_id(deployment) {
            return None;
        }
        std::fs::read_to_string(self.dir.join(deployment).join(capnp::USER_MODULE)).ok()
    }

    /// Writes a function's modules into its directory.
    fn write_modules(&self, spec: &FunctionSpec) -> std::io::Result<()> {
        let d = self.dir.join(&spec.deployment_id);
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join(capnp::USER_MODULE), &spec.code)?;
        std::fs::write(
            d.join(capnp::CONSOLE_MODULE),
            capnp::console_module(&spec.deployment_id),
        )?;
        std::fs::write(d.join(capnp::ENTRY_MODULE), capnp::ENTRY_SOURCE)?;
        self.launcher.own(&d)
    }

    /// Writes a config file readable by workerd's user only.
    fn write_config(&self, name: &str, text: &str) -> std::io::Result<PathBuf> {
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        self.launcher.own(&tmp)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    /// Starts `spec` alone in a throwaway workerd; the error has workerd's
    /// output when it doesn't come up (a syntax error, no default export,
    /// a compatibility date workerd doesn't know).
    pub async fn check(&self, spec: &FunctionSpec) -> Result<(), String> {
        if !safe_id(&spec.deployment_id) {
            return Err("invalid deployment id".into());
        }
        self.write_modules(spec)
            .map_err(|e| format!("Cannot write the function's code: {e}"))?;
        let port = free_port()
            .await
            .map_err(|e| format!("No free port to start the function on: {e}"))?;
        let name = format!("check-{}.capnp", spec.deployment_id);
        let text = capnp::render(&[service(spec, port)]);
        let config = self
            .write_config(&name, &text)
            .map_err(|e| format!("Cannot write the function's config: {e}"))?;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ports = [port];
        let result = match self.launcher.launch(&config, tx).await {
            Ok(mut p) => {
                let outcome = tokio::select! {
                    ok = listening(&ports, START_TIMEOUT) => if ok { Ok(()) } else {
                        Err(format!("The function didn't start within {}s", START_TIMEOUT.as_secs()))
                    },
                    code = p.wait() => Err(format!("The function failed to start (workerd exited with code {code})")),
                };
                p.stop().await;
                outcome
            }
            Err(e) => Err(format!("{e:#}")),
        };
        let _ = std::fs::remove_file(&config);
        result.map_err(|outcome| {
            let mut out = Vec::new();
            while let Ok((_, line)) = rx.try_recv() {
                out.push(untag(&line).1.to_owned());
            }
            let tail = out[out.len().saturating_sub(30)..].join("\n");
            if tail.trim().is_empty() {
                outcome
            } else {
                format!("{outcome}:\n{tail}")
            }
        })
    }

    /// Adds or replaces a function. Returns the version that serves it.
    pub fn upsert(&self, spec: FunctionSpec) -> std::io::Result<u64> {
        self.write_modules(&spec)?;
        let mut s = self.state.lock().expect("functions lock");
        let port = match s.entries.get(&spec.deployment_id) {
            Some(e) => e.port,
            None => {
                let used: HashSet<u16> = s.entries.values().map(|e| e.port).collect();
                PORTS
                    .clone()
                    .find(|p| !used.contains(p))
                    .ok_or_else(|| std::io::Error::other("no free function port left"))?
            }
        };
        s.entries
            .insert(spec.deployment_id.clone(), Entry { spec, port });
        s.version += 1;
        let v = s.version;
        drop(s);
        self.changed.send_replace(v);
        Ok(v)
    }

    /// Restarts the process (the server asked for a restart).
    pub fn restart(&self) -> u64 {
        let mut s = self.state.lock().expect("functions lock");
        s.version += 1;
        let v = s.version;
        drop(s);
        self.changed.send_replace(v);
        v
    }

    /// Drops every function not in `wanted`, and the files of any not
    /// wanted any more (from before a node restart too).
    pub fn retain(&self, wanted: &HashSet<&str>) {
        let mut s = self.state.lock().expect("functions lock");
        let before = s.entries.len();
        s.entries.retain(|k, _| wanted.contains(k.as_str()));
        if s.entries.len() != before {
            s.version += 1;
            let v = s.version;
            drop(s);
            self.changed.send_replace(v);
        } else {
            drop(s);
        }
        for e in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if safe_id(name) && !wanted.contains(name) && e.path().is_dir() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
        self.logs.retain(wanted);
    }

    /// Waits until the process serves `version` (or a later one).
    pub async fn wait_serving(&self, version: u64, timeout: Duration) -> Result<(), String> {
        let mut rx = self.serving.subscribe();
        let ok = tokio::time::timeout(timeout, rx.wait_for(|s| s.up && s.version >= version))
            .await
            .is_ok_and(|r| r.is_ok());
        if ok {
            Ok(())
        } else {
            Err(
                "The functions runtime didn't come up with the function: see the deployment's logs"
                    .into(),
            )
        }
    }

    /// The deployment's latest output (`tail` lines), then new lines with
    /// `follow`. None: not a function here.
    pub fn logs(
        &self,
        deployment: &str,
        tail: u32,
        follow: bool,
    ) -> Option<BoxStream<'static, anyhow::Result<LogLine>>> {
        use futures::StreamExt as _;
        if !self.contains(deployment) {
            return None;
        }
        let (recent, rx) = self.logs.subscribe(deployment, tail as usize);
        let head = futures::stream::iter(recent.into_iter().map(Ok));
        if !follow {
            return Some(head.boxed());
        }
        let dep = deployment.to_owned();
        let live = futures::stream::unfold((rx, dep), |(mut rx, dep)| async move {
            loop {
                match rx.recv().await {
                    Ok((d, l)) if d == dep => return Some((Ok(l), (rx, dep))),
                    Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        });
        Some(head.chain(live).boxed())
    }

    fn snapshot(&self) -> (u64, String, Vec<u16>) {
        let s = self.state.lock().expect("functions lock");
        let services: Vec<capnp::Service<'_>> = s
            .entries
            .values()
            .map(|e| service(&e.spec, e.port))
            .collect();
        let ports = s.entries.values().map(|e| e.port).collect();
        let text = if services.is_empty() {
            String::new()
        } else {
            capnp::render(&services)
        };
        (s.version, text, ports)
    }

    /// Supervises workerd and ships the functions' output to `outbox`
    /// until `stop`.
    pub async fn run(
        self: Arc<Self>,
        outbox: mpsc::Sender<pb::NodeMessage>,
        stop: CancellationToken,
    ) {
        let mut changed = self.changed.subscribe();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let mut proc: Option<Box<dyn process::Process>> = None;
        let mut backoff = Duration::from_secs(1);
        let mut ship = tokio::time::interval(SHIP_EVERY);
        let mut relaunch = true;
        loop {
            if relaunch {
                relaunch = false;
                if let Some(mut p) = proc.take() {
                    p.stop().await;
                }
                let (version, text, ports) = self.snapshot();
                self.set_serving(Serving { version, up: false });
                if !text.is_empty() {
                    match self.start(&text, &ports, out_tx.clone()).await {
                        Ok(p) => {
                            proc = Some(p);
                            backoff = Duration::from_secs(1);
                            self.set_serving(Serving { version, up: true });
                            tracing::info!(functions = ports.len(), "functions runtime serving");
                        }
                        Err((p, e)) => {
                            tracing::warn!(error = %e, "the functions runtime didn't start; retrying");
                            if let Some(mut p) = p {
                                p.stop().await;
                            }
                            tokio::select! {
                                _ = stop.cancelled() => break,
                                _ = tokio::time::sleep(backoff) => {}
                                _ = changed.changed() => {}
                            }
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                            relaunch = true;
                            continue;
                        }
                    }
                }
            }
            let event = {
                let exited = async {
                    match proc.as_mut() {
                        Some(p) => p.wait().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    _ = stop.cancelled() => Event::Stop,
                    _ = changed.changed() => Event::Changed,
                    code = exited => Event::Exited(code),
                    Some((stderr, line)) = out_rx.recv() => Event::Line(stderr, line),
                    _ = ship.tick() => Event::Ship,
                }
            };
            match event {
                Event::Stop => break,
                Event::Changed => {
                    tokio::time::sleep(DEBOUNCE).await;
                    let _ = changed.borrow_and_update();
                    relaunch = true;
                }
                Event::Exited(code) => {
                    tracing::warn!(code, "the functions runtime exited; restarting");
                    proc = None;
                    let version = self.serving.borrow().version;
                    self.set_serving(Serving { version, up: false });
                    tokio::select! {
                        _ = stop.cancelled() => break,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    relaunch = true;
                }
                Event::Line(stderr, line) => self.logs.push(stderr, &line),
                Event::Ship => self.logs.flush(&outbox),
            }
        }
        if let Some(mut p) = proc.take() {
            p.stop().await;
        }
    }

    /// Writes the config and starts workerd until every port listens.
    async fn start(
        &self,
        text: &str,
        ports: &[u16],
        out: mpsc::UnboundedSender<process::Line>,
    ) -> Result<Box<dyn process::Process>, (Option<Box<dyn process::Process>>, String)> {
        let config = self
            .write_config(CONFIG, text)
            .map_err(|e| (None, format!("cannot write the config: {e}")))?;
        let mut p = self
            .launcher
            .launch(&config, out)
            .await
            .map_err(|e| (None, format!("{e:#}")))?;
        let up = tokio::select! {
            ok = listening(ports, START_TIMEOUT) => ok,
            code = p.wait() => return Err((None, format!("workerd exited with code {code}"))),
        };
        if up {
            Ok(p)
        } else {
            Err((Some(p), "workerd didn't listen in time".into()))
        }
    }
}

/// What woke the supervisor.
enum Event {
    Stop,
    Changed,
    Exited(i32),
    Line(bool, String),
    Ship,
}

fn service(spec: &FunctionSpec, port: u16) -> capnp::Service<'_> {
    capnp::Service {
        name: &spec.deployment_id,
        dir: &spec.deployment_id,
        port,
        compatibility_date: &spec.compatibility_date,
        bindings: &spec.bindings,
    }
}

/// A loopback port nothing listens on right now.
async fn free_port() -> std::io::Result<u16> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    Ok(l.local_addr()?.port())
}

/// Whether every port accepts connections within `timeout`.
async fn listening(ports: &[u16], timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    for p in ports {
        loop {
            if tokio::net::TcpStream::connect(("127.0.0.1", *p))
                .await
                .is_ok()
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    true
}

/// A line of workerd output: the deployment that printed it (when tagged)
/// and its text.
pub fn untag(line: &str) -> (Option<&str>, &str) {
    if let Some(rest) = line.strip_prefix(TAG_START)
        && let Some((dep, text)) = rest.split_once(TAG_END)
    {
        return (Some(dep), text.strip_prefix(' ').unwrap_or(text));
    }
    (None, line)
}

#[derive(Default)]
struct LogState {
    ship: Vec<pb::ShippedLine>,
    dropped: u32,
    recent: HashMap<String, VecDeque<LogLine>>,
    /// Per stream (stdout, stderr): the deployment that printed last, when.
    last: [Option<(String, Instant)>; 2],
}

struct Logs {
    state: Mutex<LogState>,
    live: broadcast::Sender<(String, LogLine)>,
}

impl Default for Logs {
    fn default() -> Self {
        Self {
            state: Mutex::new(LogState::default()),
            live: broadcast::channel(1024).0,
        }
    }
}

impl Logs {
    fn push(&self, stderr: bool, line: &str) {
        let now = Instant::now();
        let mut s = self.state.lock().expect("logs lock");
        let stream = usize::from(stderr);
        let (dep, text) = match untag(line) {
            (Some(dep), text) => (dep.to_owned(), text),
            (None, text) => match &s.last[stream] {
                Some((dep, at)) if now.duration_since(*at) < CONTINUES_WITHIN => {
                    (dep.clone(), text)
                }
                _ => {
                    tracing::info!(target: "modula_node::functions::workerd", "{text}");
                    return;
                }
            },
        };
        s.last[stream] = Some((dep.clone(), now));
        let mut text = text.to_owned();
        if text.len() > MAX_LINE {
            let mut end = MAX_LINE;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push('…');
        }
        let l = LogLine {
            ts_unix_ms: now_ms(),
            stderr,
            text,
        };
        let recent = s.recent.entry(dep.clone()).or_default();
        recent.push_back(l.clone());
        while recent.len() > RECENT {
            recent.pop_front();
        }
        if s.ship.len() >= SHIP_BUFFER {
            s.dropped = s.dropped.saturating_add(1);
        } else {
            s.ship.push(pb::ShippedLine {
                deployment_id: dep.clone(),
                at_unix_ms: l.ts_unix_ms,
                stderr,
                text: l.text.clone(),
            });
        }
        drop(s);
        let _ = self.live.send((dep, l));
    }

    fn subscribe(
        &self,
        dep: &str,
        tail: usize,
    ) -> (Vec<LogLine>, broadcast::Receiver<(String, LogLine)>) {
        let s = self.state.lock().expect("logs lock");
        let recent = s
            .recent
            .get(dep)
            .map(|r| {
                r.iter()
                    .skip(r.len().saturating_sub(tail))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        (recent, self.live.subscribe())
    }

    fn retain(&self, wanted: &HashSet<&str>) {
        let mut s = self.state.lock().expect("logs lock");
        s.recent.retain(|k, _| wanted.contains(k.as_str()));
    }

    /// Sends what is buffered as one `LogBatch`, leaving half the outbox
    /// to everything else (like container output, see `crate::logship`).
    fn flush(&self, outbox: &mpsc::Sender<pb::NodeMessage>) {
        if outbox.capacity() * 2 < outbox.max_capacity() {
            return;
        }
        let batch = {
            let mut s = self.state.lock().expect("logs lock");
            if s.ship.is_empty() && s.dropped == 0 {
                return;
            }
            let n = s.ship.len().min(5_000);
            pb::LogBatch {
                lines: s.ship.drain(..n).collect(),
                dropped: std::mem::take(&mut s.dropped),
            }
        };
        let m = pb::NodeMessage {
            msg: Some(pb::node_message::Msg::LogBatch(batch)),
        };
        if let Err(mpsc::error::TrySendError::Full(m) | mpsc::error::TrySendError::Closed(m)) =
            outbox.try_send(m)
            && let Some(pb::node_message::Msg::LogBatch(b)) = m.msg
        {
            let mut s = self.state.lock().expect("logs lock");
            let rest = std::mem::take(&mut s.ship);
            s.ship = b.lines;
            s.ship.extend(rest);
            s.dropped = s.dropped.saturating_add(b.dropped);
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use modula_id::Id;

    fn spec(dep: &str, code: &str) -> FunctionSpec {
        FunctionSpec {
            deployment_id: dep.into(),
            compatibility_date: "2026-10-01".into(),
            code: code.into(),
            bindings: vec![("GREETING".into(), "hi".into())],
        }
    }

    fn setup() -> (Arc<Functions>, Arc<FakeWorkerd>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("modula-functions-{}", Id::new()));
        let fake = Arc::new(FakeWorkerd::default());
        let f = Functions::new(dir.clone(), fake.clone()).unwrap();
        (f, fake, dir)
    }

    #[test]
    fn bindings_are_the_variables_that_can_be() {
        let w = pb::Workload {
            deployment_id: Id::new().to_string(),
            env: vec![
                pb::EnvVar {
                    name: "API_URL".into(),
                    value: "https://x".into(),
                },
                pb::EnvVar {
                    name: "BAD-NAME".into(),
                    value: "x".into(),
                },
                pb::EnvVar {
                    name: "NUL".into(),
                    value: "a\0b".into(),
                },
                pb::EnvVar {
                    name: "_OK1".into(),
                    value: "y".into(),
                },
            ],
            ..Default::default()
        };
        let f = pb::Function {
            code: String::new(),
            compatibility_date: "2026-10-01".into(),
        };
        let s = FunctionSpec::new(&w, &f, "x".into());
        let names: Vec<&str> = s.bindings.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["API_URL", "_OK1"]);
    }

    #[test]
    fn lines_are_attributed_by_their_tag() {
        assert_eq!(
            untag("\u{1e}d1\u{1f} hello world"),
            (Some("d1"), "hello world")
        );
        assert_eq!(untag("plain"), (None, "plain"));
        let logs = Logs::default();
        logs.push(false, "\u{1e}d1\u{1f} first");
        logs.push(false, "continued");
        logs.push(true, "workerd's own line");
        let (recent, _) = logs.subscribe("d1", 10);
        let texts: Vec<&str> = recent.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["first", "continued"]);
        let s = logs.state.lock().unwrap();
        assert_eq!(s.ship.len(), 2);
        assert!(s.ship.iter().all(|l| l.deployment_id == "d1"));
    }

    /// The supervisor (re)starts workerd whenever the functions change, with
    /// every function in one config, and again after it died.
    #[tokio::test]
    async fn the_supervisor_restarts_workerd_on_changes() {
        let (f, fake, dir) = setup();
        let (out, _rx) = mpsc::channel(64);
        let stop = CancellationToken::new();
        tokio::spawn(f.clone().run(out, stop.clone()));
        let (a, b) = (Id::new().to_string(), Id::new().to_string());

        f.check(&spec(&a, "export default {}")).await.unwrap();
        let v = f.upsert(spec(&a, "export default {}")).unwrap();
        f.wait_serving(v, Duration::from_secs(5)).await.unwrap();
        assert!(f.up());
        let pa = f.port(&a).unwrap();
        assert!(PORTS.contains(&pa));
        assert!(
            tokio::net::TcpStream::connect(("127.0.0.1", pa))
                .await
                .is_ok()
        );

        let v = f.upsert(spec(&b, "export default {}")).unwrap();
        f.wait_serving(v, Duration::from_secs(5)).await.unwrap();
        let launches = fake.launches();
        // The check, then one process per version.
        assert_eq!(launches.len(), 3, "{launches:#?}");
        let last = launches.last().unwrap();
        assert!(
            last.contains(&format!("(name = \"{a}\""))
                && last.contains(&format!("(name = \"{b}\""))
        );
        assert_eq!(*fake.running.lock().unwrap(), 1, "the old process stopped");
        assert_ne!(f.port(&b), Some(pa), "ports stay put and differ");
        assert_eq!(f.port(&a), Some(pa));
        assert!(dir.join(&a).join("worker.js").is_file());

        // Removing one rewrites the config without it and deletes its files.
        f.retain(&HashSet::from([b.as_str()]));
        tokio::time::timeout(Duration::from_secs(5), async {
            while fake.launches().len() < 4 || !f.up() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(!fake.launches().last().unwrap().contains(&a));
        assert!(!dir.join(&a).exists());
        assert!(f.port(&a).is_none());
        let config = std::fs::read_to_string(dir.join(CONFIG)).unwrap();
        assert!(config.contains("GREETING"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.join(CONFIG))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "variables are workerd's user's alone");
        }

        // Nothing left: the process stops.
        f.retain(&HashSet::new());
        tokio::time::timeout(Duration::from_secs(5), async {
            while *fake.running.lock().unwrap() > 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        stop.cancel();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_function_that_fails_to_load_never_joins() {
        let (f, fake, dir) = setup();
        let d = Id::new().to_string();
        let err = f
            .check(&spec(
                &d,
                &format!("export default {{ {} ", process::FAIL_MARK),
            ))
            .await
            .unwrap_err();
        assert!(
            err.contains("failed to start") && err.contains("SyntaxError"),
            "{err}"
        );
        assert_eq!(fake.launches().len(), 1);
        assert!(!f.contains(&d));
        assert!(!dir.join(format!("check-{d}.capnp")).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn logs_follow_a_function() {
        use futures::StreamExt as _;
        let (f, _fake, dir) = setup();
        let d = Id::new().to_string();
        f.upsert(spec(&d, "export default {}")).unwrap();
        f.logs.push(false, &format!("\u{1e}{d}\u{1f} one"));
        let mut s = f.logs(&d, 10, true).unwrap();
        assert_eq!(s.next().await.unwrap().unwrap().text, "one");
        f.logs.push(true, &format!("\u{1e}{d}\u{1f} two"));
        let l = s.next().await.unwrap().unwrap();
        assert_eq!((l.text.as_str(), l.stderr), ("two", true));
        assert!(f.logs(&Id::new().to_string(), 10, false).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
