import { type InstanceMetrics, type MetricsRange } from "@/lib/api";
import { formatBytes } from "@/lib/format";
import { formatRate, formatVcpu, niceMax } from "@/lib/metrics";
import { MetricsChart } from "./MetricsChart";

export const RANGE_SECS: Record<MetricsRange, number> = { "1h": 3600, "6h": 6 * 3600, "24h": 24 * 3600, "7d": 7 * 24 * 3600 };

/** CPU, memory and network charts for one metrics response (an instance, or a whole project). */
export function MetricsPanels({ m, range }: { m: InstanceMetrics; range: MetricsRange }) {
  const pts = m.points;
  const last = pts[pts.length - 1];
  if (!last) return null;

  // The window ends at the newest sample (or now when idle) so axes don't jump around.
  const to = Math.max(last.t, Date.now() - m.stepSecs * 1000);
  const from = to - RANGE_SECS[range] * 1000;
  const maxGapMs = m.stepSecs * 2000;

  const vcpu = pts.map((p) => ({ t: p.t, v: p.cpuPercent / 100 }));
  const cpuDataMax = Math.max(0, ...vcpu.map((p) => p.v));
  const cpus = m.cpus;
  const cpuNearLimit = cpus > 0 && cpuDataMax >= cpus * 0.7;
  const cpuMax = niceMax(Math.max(1, cpuDataMax, cpuNearLimit ? cpus : 0));

  const mem = pts.map((p) => ({ t: p.t, v: p.memoryBytes }));
  const memLimit = m.memoryLimitBytes;
  const memMax = niceMax(Math.max(...mem.map((p) => p.v), memLimit ?? 0, 1));

  const rx = pts.map((p) => ({ t: p.t, v: p.netRxBytesPerSec }));
  const tx = pts.map((p) => ({ t: p.t, v: p.netTxBytesPerSec }));
  const netMax = niceMax(Math.max(...rx.map((p) => p.v), ...tx.map((p) => p.v), 1));

  const common = { range, from, to, maxGapMs };

  return (
    <>
      <MetricsChart
        {...common}
        title="CPU"
        latest={formatVcpu(last.cpuPercent / 100)}
        series={[{ name: "CPU", color: "var(--chart-1)", data: vcpu }]}
        max={cpuMax}
        yLabel={formatVcpu}
        valueLabel={formatVcpu}
        reference={cpuNearLimit ? { value: cpus, label: `${cpus} vCPU on this server` } : undefined}
      />
      <MetricsChart
        {...common}
        title="Memory"
        latest={formatBytes(last.memoryBytes)}
        series={[{ name: "Memory", color: "var(--chart-1)", data: mem }]}
        max={memMax}
        yLabel={formatBytes}
        valueLabel={formatBytes}
        reference={memLimit ? { value: memLimit, label: `Limit ${formatBytes(memLimit)}` } : undefined}
      />
      <MetricsChart
        {...common}
        title="Network"
        latest={`↓ ${formatRate(last.netRxBytesPerSec)}  ↑ ${formatRate(last.netTxBytesPerSec)}`}
        series={[
          { name: "In", color: "var(--chart-1)", data: rx },
          { name: "Out", color: "var(--chart-4)", data: tx },
        ]}
        max={netMax}
        yLabel={formatRate}
        valueLabel={formatRate}
      />
    </>
  );
}
