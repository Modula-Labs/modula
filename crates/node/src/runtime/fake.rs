//! An in-memory runtime for tests: no Docker, deterministic, scriptable.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use modula_proto::node::v1::Workload;

use super::{Container, Exec, LogLine, OutputSink, Runtime};

#[derive(Default)]
struct State {
    networks: BTreeSet<String>,
    containers: BTreeMap<String, Container>,
    /// Environment per container, so logs can echo it (tests redaction).
    env: BTreeMap<String, Vec<(String, String)>>,
    removed_volumes: BTreeSet<String>,
    /// container id → (volume name, mount path)
    mounts: BTreeMap<String, (String, String)>,
    /// container id → the workload it was created from.
    workloads: BTreeMap<String, Workload>,
    next: u64,
}

#[derive(Default)]
pub struct FakeRuntime {
    state: Mutex<State>,
    /// Images whose pull fails.
    pub unpullable: Mutex<HashSet<String>>,
    /// Deployments whose health probe never passes (`*`: every deployment).
    pub unhealthy: Mutex<HashSet<String>>,
    /// What commands run with `exec_output` print (database dumps).
    pub exec_stdout: Mutex<Vec<u8>>,
    /// Every command run in a container, in order.
    pub execs: Mutex<Vec<ExecCall>>,
    /// Exit codes of one-off commands (`run_once`), by argv joined with
    /// spaces; anything else exits 0.
    pub once_exit: Mutex<HashMap<String, i64>>,
    /// Every one-off command run, in order.
    pub onces: Mutex<Vec<OnceCall>>,
    /// Report this as every container's address (e.g. 127.0.0.1, so tunnels
    /// reach a test server); `None` = a made-up 10.88.0.x per container.
    pub address_override: Mutex<Option<std::net::IpAddr>>,
}

/// A command run inside a fake container.
#[derive(Debug, Clone)]
pub struct ExecCall {
    pub deployment_id: String,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    /// What it was fed (`exec_input` only).
    pub stdin: Option<Vec<u8>>,
}

/// A one-off container run with `run_once`.
#[derive(Debug, Clone)]
pub struct OnceCall {
    pub deployment_id: String,
    pub image: String,
    pub cmd: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub network: String,
    /// Deployments whose containers were running at the time.
    pub running: BTreeSet<String>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Simulate a crash: the container exits with `code`.
    pub fn crash(&self, deployment_id: &str, code: i64) {
        let mut s = self.state.lock().expect("fake lock");
        for c in s
            .containers
            .values_mut()
            .filter(|c| c.deployment_id == deployment_id)
        {
            c.running = false;
            c.exit_code = code;
        }
    }

    /// Simulate the restart policy bringing a container back.
    pub fn recover(&self, deployment_id: &str) {
        let mut s = self.state.lock().expect("fake lock");
        for c in s
            .containers
            .values_mut()
            .filter(|c| c.deployment_id == deployment_id)
        {
            c.running = true;
            c.restart_count += 1;
        }
    }

    /// The volume a deployment's container mounts: (name, path).
    pub fn mount_of(&self, deployment_id: &str) -> Option<(String, String)> {
        let s = self.state.lock().expect("fake lock");
        s.containers
            .iter()
            .find(|(_, c)| c.deployment_id == deployment_id)
            .and_then(|(id, _)| s.mounts.get(id).cloned())
    }

    pub fn removed_volumes(&self) -> BTreeSet<String> {
        self.state
            .lock()
            .expect("fake lock")
            .removed_volumes
            .clone()
    }

    /// The workload a deployment's container was created from.
    pub fn workload_of(&self, deployment_id: &str) -> Option<Workload> {
        let s = self.state.lock().expect("fake lock");
        s.containers
            .iter()
            .find(|(_, c)| c.deployment_id == deployment_id)
            .and_then(|(id, _)| s.workloads.get(id).cloned())
    }

    /// The environment a deployment's container was created with.
    pub fn env_of(&self, deployment_id: &str) -> BTreeMap<String, String> {
        let s = self.state.lock().expect("fake lock");
        s.containers
            .iter()
            .find(|(_, c)| c.deployment_id == deployment_id)
            .and_then(|(id, _)| s.env.get(id))
            .map(|e| e.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn running_deployments(&self) -> BTreeSet<String> {
        let s = self.state.lock().expect("fake lock");
        s.containers
            .values()
            .filter(|c| c.running)
            .map(|c| c.deployment_id.clone())
            .collect()
    }

    pub fn network_ids(&self) -> BTreeSet<String> {
        self.state.lock().expect("fake lock").networks.clone()
    }
}

#[async_trait]
impl Runtime for FakeRuntime {
    async fn describe(&self) -> anyhow::Result<String> {
        Ok("fake 1.0".into())
    }

    async fn ensure_network(&self, environment_id: &str) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("fake lock")
            .networks
            .insert(environment_id.to_owned());
        Ok(())
    }

    async fn remove_network(&self, environment_id: &str) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("fake lock")
            .networks
            .remove(environment_id);
        Ok(())
    }

    async fn networks(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.network_ids().into_iter().collect())
    }

    async fn containers(&self) -> anyhow::Result<Vec<Container>> {
        Ok(self
            .state
            .lock()
            .expect("fake lock")
            .containers
            .values()
            .cloned()
            .collect())
    }

    async fn pull(&self, image: &str) -> anyhow::Result<()> {
        if self.unpullable.lock().expect("fake lock").contains(image) {
            anyhow::bail!("pull access denied for {image}");
        }
        Ok(())
    }

    async fn create(&self, w: &Workload) -> anyhow::Result<String> {
        let mut s = self.state.lock().expect("fake lock");
        anyhow::ensure!(
            s.networks.contains(&w.environment_id),
            "network for {} is missing",
            w.environment_id
        );
        s.next += 1;
        let id = format!("fake-{}", s.next);
        if let Some(v) = &w.volume {
            s.mounts
                .insert(id.clone(), (v.name.clone(), v.mount_path.clone()));
        }
        s.workloads.insert(id.clone(), w.clone());
        s.env.insert(
            id.clone(),
            w.env
                .iter()
                .map(|e| (e.name.clone(), e.value.clone()))
                .collect(),
        );
        s.containers.insert(
            id.clone(),
            Container {
                id: id.clone(),
                deployment_id: w.deployment_id.clone(),
                service_id: w.service_id.clone(),
                environment_id: w.environment_id.clone(),
                running: false,
                exit_code: 0,
                restart_count: 0,
                mesh_ip: w.mesh_ip.parse().ok(),
            },
        );
        Ok(id)
    }

    async fn start(&self, container_id: &str) -> anyhow::Result<()> {
        let mut s = self.state.lock().expect("fake lock");
        let c = s
            .containers
            .get_mut(container_id)
            .ok_or_else(|| anyhow::anyhow!("no such container"))?;
        c.running = true;
        Ok(())
    }

    /// Prints one line naming the command, then exits with the scripted code.
    async fn run_once(
        &self,
        w: &Workload,
        command: &[String],
        _timeout: std::time::Duration,
        output: OutputSink,
    ) -> anyhow::Result<i64> {
        let running = {
            let s = self.state.lock().expect("fake lock");
            anyhow::ensure!(
                s.networks.contains(&w.environment_id),
                "network for {} is missing",
                w.environment_id
            );
            s.containers
                .values()
                .filter(|c| c.running)
                .map(|c| c.deployment_id.clone())
                .collect()
        };
        let joined = command.join(" ");
        self.onces.lock().expect("fake lock").push(OnceCall {
            deployment_id: w.deployment_id.clone(),
            image: w.image.clone(),
            cmd: command.to_vec(),
            env: w
                .env
                .iter()
                .map(|e| (e.name.clone(), e.value.clone()))
                .collect(),
            network: w.environment_id.clone(),
            running,
        });
        let code = self
            .once_exit
            .lock()
            .expect("fake lock")
            .get(&joined)
            .copied()
            .unwrap_or(0);
        output(LogLine {
            ts_unix_ms: 0,
            stderr: code != 0,
            text: format!("ran {joined}"),
        });
        Ok(code)
    }

    async fn restart(&self, container_id: &str) -> anyhow::Result<()> {
        let mut s = self.state.lock().expect("fake lock");
        let c = s
            .containers
            .get_mut(container_id)
            .ok_or_else(|| anyhow::anyhow!("no such container"))?;
        c.running = true;
        c.restart_count += 1;
        Ok(())
    }

    async fn remove_volume(&self, name: &str) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("fake lock")
            .removed_volumes
            .insert(name.to_owned());
        Ok(())
    }

    async fn remove(&self, container_id: &str) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("fake lock")
            .containers
            .remove(container_id);
        Ok(())
    }

    async fn address(
        &self,
        container_id: &str,
        _env: &str,
    ) -> anyhow::Result<Option<std::net::IpAddr>> {
        if let Some(ip) = *self.address_override.lock().expect("fake lock") {
            return Ok(Some(ip));
        }
        let n: u8 = container_id
            .trim_start_matches("fake-")
            .parse()
            .unwrap_or(0);
        Ok(Some(std::net::IpAddr::from([10, 88, 0, n])))
    }

    async fn probe(
        &self,
        container_id: &str,
        _env: &str,
        _port: u16,
        _path: &str,
    ) -> anyhow::Result<bool> {
        let s = self.state.lock().expect("fake lock");
        let c = s
            .containers
            .get(container_id)
            .ok_or_else(|| anyhow::anyhow!("no such container"))?;
        let unhealthy = self.unhealthy.lock().expect("fake lock");
        Ok(c.running && !unhealthy.contains(&c.deployment_id) && !unhealthy.contains("*"))
    }

    /// A startup line followed by one `NAME=value` line per variable, like an
    /// app that prints its environment (which is what redaction must catch).
    async fn logs(
        &self,
        container_id: &str,
        tail: u32,
        _follow: bool,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<LogLine>>> {
        let s = self.state.lock().expect("fake lock");
        let mut texts = vec![format!("{container_id} started")];
        texts.extend(
            s.env
                .get(container_id)
                .into_iter()
                .flatten()
                .map(|(k, v)| format!("{k}={v}")),
        );
        let skip = texts.len().saturating_sub(tail as usize);
        let lines: Vec<anyhow::Result<LogLine>> = texts
            .into_iter()
            .skip(skip)
            .enumerate()
            .map(|(i, text)| {
                Ok(LogLine {
                    ts_unix_ms: i as i64,
                    stderr: false,
                    text,
                })
            })
            .collect();
        Ok(futures::stream::iter(lines).boxed())
    }

    async fn container_env(&self, container_id: &str) -> anyhow::Result<Vec<(String, String)>> {
        let s = self.state.lock().expect("fake lock");
        s.env
            .get(container_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such container"))
    }

    async fn exec_output(
        &self,
        container_id: &str,
        exec: Exec,
    ) -> anyhow::Result<BoxStream<'static, anyhow::Result<bytes::Bytes>>> {
        self.record_exec(container_id, exec, None)?;
        let out = bytes::Bytes::from(self.exec_stdout.lock().expect("fake lock").clone());
        // Two chunks, like a real stream.
        let mid = out.len() / 2;
        let chunks = vec![Ok(out.slice(..mid)), Ok(out.slice(mid..))];
        Ok(futures::stream::iter(chunks).boxed())
    }

    async fn exec_input(
        &self,
        container_id: &str,
        exec: Exec,
        mut stdin: BoxStream<'static, anyhow::Result<bytes::Bytes>>,
    ) -> anyhow::Result<()> {
        let mut fed = Vec::new();
        while let Some(chunk) = stdin.next().await {
            fed.extend_from_slice(&chunk?);
        }
        self.record_exec(container_id, exec, Some(fed))
    }
}

impl FakeRuntime {
    fn record_exec(
        &self,
        container_id: &str,
        exec: Exec,
        stdin: Option<Vec<u8>>,
    ) -> anyhow::Result<()> {
        let deployment_id = {
            let s = self.state.lock().expect("fake lock");
            let c = s
                .containers
                .get(container_id)
                .ok_or_else(|| anyhow::anyhow!("no such container"))?;
            anyhow::ensure!(c.running, "container {container_id} is not running");
            c.deployment_id.clone()
        };
        self.execs.lock().expect("fake lock").push(ExecCall {
            deployment_id,
            cmd: exec.cmd,
            env: exec.env,
            stdin,
        });
        Ok(())
    }
}
