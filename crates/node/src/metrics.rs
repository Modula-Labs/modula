//! Resource usage of the running workloads and of the server itself (CPU,
//! memory, network), sampled every [`EVERY`] and sent to the server, which
//! keeps a week of it for the dashboard's charts.

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
    host: HostSampler,
}

impl Sampler {
    pub fn new(runtime: Arc<dyn Runtime>) -> Self {
        Self {
            runtime,
            previous: HashMap::new(),
            host: HostSampler::default(),
        }
    }

    /// One sample of the server and every running workload (no workloads
    /// when none run).
    pub async fn sample(&mut self) -> pb::MetricsReport {
        let at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        let host = self.host.sample(at_unix_ms);
        let Ok(containers) = self.runtime.containers().await else {
            return pb::MetricsReport {
                workloads: Vec::new(),
                host,
            };
        };
        let running: Vec<_> = containers
            .into_iter()
            .filter(|c| c.running && !c.deployment_id.is_empty())
            .collect();
        let samples =
            futures::future::join_all(running.iter().map(|c| self.runtime.stats(&c.id))).await;
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
        pb::MetricsReport { workloads, host }
    }
}

/// The whole server's usage, from `/proc` (Linux only; elsewhere nothing).
/// CPU and network are rates, so the first sample only sets the baseline.
#[derive(Default)]
struct HostSampler {
    #[cfg(target_os = "linux")]
    previous: Option<HostTotals>,
}

/// The counters rates are measured against.
#[cfg(target_os = "linux")]
struct HostTotals {
    at: Instant,
    /// (busy, total) jiffies of all cores.
    cpu: (u64, u64),
    /// (rx, tx) bytes of the physical interfaces.
    net: (u64, u64),
}

impl HostSampler {
    #[cfg(target_os = "linux")]
    fn sample(&mut self, at_unix_ms: i64) -> Option<pb::HostStats> {
        let read = |path: &str| std::fs::read_to_string(path).ok();
        let cpu = proc::cpu(&read("/proc/stat")?)?;
        let (memory_used_bytes, memory_total_bytes) = proc::memory(&read("/proc/meminfo")?)?;
        let load1 = proc::load1(&read("/proc/loadavg")?)?;
        let net = proc::network(&read("/proc/net/dev")?);
        let now = Instant::now();
        let prev = self.previous.replace(HostTotals { at: now, cpu, net })?;
        let secs = now.duration_since(prev.at).as_secs_f64().max(1.0);
        let busy = cpu.0.saturating_sub(prev.cpu.0) as f64;
        let total = cpu.1.saturating_sub(prev.cpu.1) as f64;
        Some(pb::HostStats {
            at_unix_ms,
            cpu_percent: if total > 0.0 {
                (busy / total * 100.0).min(100.0)
            } else {
                0.0
            },
            memory_used_bytes,
            memory_total_bytes,
            load1,
            net_rx_bytes_per_sec: net.0.saturating_sub(prev.net.0) as f64 / secs,
            net_tx_bytes_per_sec: net.1.saturating_sub(prev.net.1) as f64 / secs,
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn sample(&mut self, _at_unix_ms: i64) -> Option<pb::HostStats> {
        None
    }
}

/// Parsers for the `/proc` files [`HostSampler`] reads.
#[cfg(any(target_os = "linux", test))]
mod proc {
    /// `/proc/stat`: all cores' (busy, total) jiffies since boot. Guest
    /// time is already counted in user time, so only the first eight
    /// columns add up; idle and iowait are not busy.
    pub fn cpu(stat: &str) -> Option<(u64, u64)> {
        let line = stat.lines().find(|l| l.starts_with("cpu "))?;
        let v: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .take(8)
            .map(|x| x.parse().ok())
            .collect::<Option<_>>()?;
        if v.len() < 4 {
            return None;
        }
        let total: u64 = v.iter().sum();
        let idle = v[3] + v.get(4).copied().unwrap_or(0);
        Some((total - idle, total))
    }

    /// `/proc/meminfo`: (used, total) bytes, used being what is not
    /// available (page cache that can be dropped is available).
    pub fn memory(meminfo: &str) -> Option<(u64, u64)> {
        let kb = |key: &str| -> Option<u64> {
            let rest = meminfo.lines().find_map(|l| l.strip_prefix(key))?;
            rest.trim_start_matches(':')
                .split_whitespace()
                .next()?
                .parse::<u64>()
                .ok()
        };
        let total = kb("MemTotal:")? * 1024;
        let available = kb("MemAvailable:")? * 1024;
        Some((total.saturating_sub(available), total))
    }

    /// `/proc/loadavg`: the 1-minute load average.
    pub fn load1(loadavg: &str) -> Option<f64> {
        loadavg.split_whitespace().next()?.parse().ok()
    }

    /// Interfaces that are not the server's own link to the network:
    /// loopback, Docker's bridges and container ends, and the mesh
    /// (WireGuard and its bridge).
    fn virtual_interface(name: &str) -> bool {
        name == "lo"
            || ["docker", "veth", "br-", "wg", "modula"]
                .iter()
                .any(|p| name.starts_with(p))
    }

    /// `/proc/net/dev`: (rx, tx) byte totals of the physical interfaces.
    pub fn network(dev: &str) -> (u64, u64) {
        let (mut rx, mut tx) = (0u64, 0u64);
        for line in dev.lines().skip(2) {
            let Some((name, counters)) = line.split_once(':') else {
                continue;
            };
            if virtual_interface(name.trim()) {
                continue;
            }
            let c: Vec<u64> = counters
                .split_whitespace()
                .filter_map(|x| x.parse().ok())
                .collect();
            if let (Some(r), Some(t)) = (c.first(), c.get(8)) {
                rx = rx.saturating_add(*r);
                tx = tx.saturating_add(*t);
            }
        }
        (rx, tx)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_cpu_memory_and_load() {
            let stat = "cpu  100 5 50 800 20 3 2 0 40 0\ncpu0 50 2 25 400 10 1 1 0 20 0\n";
            // busy = user + nice + system + irq + softirq + steal.
            assert_eq!(cpu(stat), Some((160, 980)));
            assert_eq!(cpu("intr 1 2 3"), None);
            let meminfo = "MemTotal:        2000000 kB\nMemFree:          100000 kB\nMemAvailable:     500000 kB\n";
            assert_eq!(memory(meminfo), Some((1_500_000 * 1024, 2_000_000 * 1024)));
            assert_eq!(load1("0.42 0.30 0.20 1/123 4567\n"), Some(0.42));
        }

        #[test]
        fn sums_only_physical_interfaces() {
            let dev = "Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  999 1 0 0 0 0 0 0  999 1 0 0 0 0 0 0
  eth0: 1000 10 0 0 0 0 0 0  2000 20 0 0 0 0 0 0
  ens10: 30 1 0 0 0 0 0 0  40 1 0 0 0 0 0 0
docker0: 500 5 0 0 0 0 0 0  500 5 0 0 0 0 0 0
veth12ab: 7 1 0 0 0 0 0 0  7 1 0 0 0 0 0 0
br-1234: 7 1 0 0 0 0 0 0  7 1 0 0 0 0 0 0
modula0: 7 1 0 0 0 0 0 0  7 1 0 0 0 0 0 0
modula-mesh: 7 1 0 0 0 0 0 0  7 1 0 0 0 0 0 0
";
            assert_eq!(network(dev), (1030, 2040));
        }
    }
}
