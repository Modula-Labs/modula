//! An in-memory runtime for tests: no Docker, deterministic, scriptable.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use modula_proto::node::v1::Workload;

use super::{Container, LogLine, Runtime};

#[derive(Default)]
struct State {
    networks: BTreeSet<String>,
    containers: BTreeMap<String, Container>,
    /// Environment per container, so logs can echo it (tests redaction).
    env: BTreeMap<String, Vec<(String, String)>>,
    next: u64,
}

#[derive(Default)]
pub struct FakeRuntime {
    state: Mutex<State>,
    /// Images whose pull fails.
    pub unpullable: Mutex<HashSet<String>>,
    /// Deployments whose health probe never passes.
    pub unhealthy: Mutex<HashSet<String>>,
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
                environment_id: w.environment_id.clone(),
                running: false,
                exit_code: 0,
                restart_count: 0,
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

    async fn remove(&self, container_id: &str) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("fake lock")
            .containers
            .remove(container_id);
        Ok(())
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
        Ok(c.running
            && !self
                .unhealthy
                .lock()
                .expect("fake lock")
                .contains(&c.deployment_id))
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
}
