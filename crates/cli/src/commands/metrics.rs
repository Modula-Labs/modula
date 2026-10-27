//! `metrics`: the service's CPU, memory and network usage (the dashboard's
//! Metrics tab): now, the range's average and peak, and a sparkline.

use serde::{Deserialize, Serialize};

use crate::context::Ctx;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metrics {
    pub range: String,
    pub step_secs: i64,
    pub cpus: i64,
    pub memory_total_bytes: i64,
    pub memory_limit_bytes: Option<i64>,
    pub points: Vec<Point>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub t: i64,
    pub cpu_percent: f64,
    pub memory_bytes: i64,
    pub net_rx_bytes_per_sec: f64,
    pub net_tx_bytes_per_sec: f64,
}

pub async fn run(ctx: &Ctx, range: &str) -> anyhow::Result<i32> {
    let t = ctx.target().await?;
    let m: Metrics = ctx
        .api()?
        .get(&format!(
            "/api/service-instances/{}/metrics?range={range}",
            t.instance
        ))
        .await?;
    ctx.out.either(&m, |o| {
        if m.points.is_empty() {
            o.line(format!(
                "No metrics for {} yet; they appear a minute after it starts.",
                t.label()
            ));
            return;
        }
        let cpu: Vec<f64> = m.points.iter().map(|p| p.cpu_percent / 100.0).collect();
        let mem: Vec<f64> = m.points.iter().map(|p| p.memory_bytes as f64).collect();
        let rx: Vec<f64> = m.points.iter().map(|p| p.net_rx_bytes_per_sec).collect();
        let tx: Vec<f64> = m.points.iter().map(|p| p.net_tx_bytes_per_sec).collect();
        o.line(format!("{} · last {}", t.label(), m.range));
        o.line(String::new());
        let of = |total: String| format!(" of {total}");
        let row = |name: &str, v: &[f64], fmt: &dyn Fn(f64) -> String, total: String| {
            format!(
                "{name:<8} {:>10}  avg {:>10}  peak {:>10}  {}{total}",
                fmt(*v.last().unwrap_or(&0.0)),
                fmt(v.iter().sum::<f64>() / v.len().max(1) as f64),
                fmt(v.iter().copied().fold(0.0, f64::max)),
                sparkline(v),
            )
        };
        o.line(row(
            "CPU",
            &cpu,
            &|v| format!("{v:.2} vCPU"),
            if m.cpus > 0 {
                of(format!("{}", m.cpus))
            } else {
                String::new()
            },
        ));
        let mem_total = m.memory_limit_bytes.unwrap_or(m.memory_total_bytes);
        o.line(row(
            "Memory",
            &mem,
            &|v| bytes(v),
            if mem_total > 0 {
                of(bytes(mem_total as f64))
            } else {
                String::new()
            },
        ));
        o.line(row(
            "Net in",
            &rx,
            &|v| format!("{}/s", bytes(v)),
            String::new(),
        ));
        o.line(row(
            "Net out",
            &tx,
            &|v| format!("{}/s", bytes(v)),
            String::new(),
        ));
    })?;
    Ok(0)
}

/// 1536 → "1.5 KB" (decimal-ish binary units, like the dashboard).
pub fn bytes(v: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = v.max(0.0);
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// The last 24 values as ▁▂▃▄▅▆▇█, scaled to their max.
pub fn sparkline(v: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let tail = &v[v.len().saturating_sub(24)..];
    let max = tail.iter().copied().fold(0.0, f64::max);
    tail.iter()
        .map(|x| {
            if max <= 0.0 {
                BARS[0]
            } else {
                BARS[((x / max) * 7.0).round().clamp(0.0, 7.0) as usize]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(bytes(512.0), "512 B");
        assert_eq!(bytes(1536.0), "1.5 KB");
        assert_eq!(bytes(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 GB");
        assert_eq!(sparkline(&[0.0, 1.0, 2.0]), "▁▅█");
        assert_eq!(sparkline(&[0.0, 0.0]), "▁▁");
    }
}
