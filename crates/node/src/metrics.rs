//! Resource usage of the running workloads (CPU, memory, network), sampled
//! every [`EVERY`] and sent to the server, which keeps a week of it for the
//! dashboard's charts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use modula_proto::node::v1 as pb;

use crate::runtime::Runtime;

pub const EVERY: Duration = Duration::from_secs(30);

/// Keeps each container's previous network totals to turn them into rates.
pub struct Sampler {
    runtime: Arc<dyn Runtime>,
    previous: HashMap<String, (Instant, u64, u64)>,
}

impl Sampler {
    pub fn new(runtime: Arc<dyn Runtime>) -> Self {
        Self {
            runtime,
            previous: HashMap::new(),
        }
    }

    /// One sample of every running workload (empty when none run).
    pub async fn sample(&mut self) -> pb::MetricsReport {
        let Ok(containers) = self.runtime.containers().await else {
            return pb::MetricsReport::default();
        };
        let running: Vec<_> = containers
            .into_iter()
            .filter(|c| c.running && !c.deployment_id.is_empty())
            .collect();
        let samples =
            futures::future::join_all(running.iter().map(|c| self.runtime.stats(&c.id))).await;
        let at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let now = Instant::now();
        let mut workloads = Vec::new();
        let mut seen = HashMap::new();
        for (c, s) in running.iter().zip(samples) {
            let Ok(Some(s)) = s else { continue };
            let (rx, tx) = match self.previous.get(&c.id) {
                Some((at, rx, tx)) => {
                    let secs = now.duration_since(*at).as_secs_f64().max(1.0);
                    (
                        s.net_rx_bytes.saturating_sub(*rx) as f64 / secs,
                        s.net_tx_bytes.saturating_sub(*tx) as f64 / secs,
                    )
                }
                None => (0.0, 0.0),
            };
            seen.insert(c.id.clone(), (now, s.net_rx_bytes, s.net_tx_bytes));
            workloads.push(pb::WorkloadMetrics {
                deployment_id: c.deployment_id.clone(),
                at_unix_ms,
                cpu_percent: s.cpu_percent,
                memory_bytes: s.memory_bytes,
                net_rx_bytes_per_sec: rx,
                net_tx_bytes_per_sec: tx,
            });
        }
        // Containers that are gone are forgotten.
        self.previous = seen;
        pb::MetricsReport { workloads }
    }
}
