import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Segmented } from "@/components/ui/Tabs";
import { Alert, Skeleton } from "@/components/ui/misc";
import { api, type MetricsRange } from "@/lib/api";
import { formatBytes } from "@/lib/format";
import { METRICS_RANGES, formatRate, formatVcpu, niceMax } from "@/lib/metrics";
import { MetricsChart } from "./MetricsChart";

const RANGE_SECS: Record<MetricsRange, number> = { "1h": 3600, "6h": 6 * 3600, "24h": 24 * 3600, "7d": 7 * 24 * 3600 };

export function MetricsTab({ instanceId }: { instanceId: string }) {
  const [range, setRange] = useState<MetricsRange>("1h");
  const q = useQuery({
    queryKey: ["metrics", instanceId, range],
    queryFn: () => api.instanceMetrics(instanceId, range),
    refetchInterval: range === "1h" || range === "6h" ? 30_000 : false,
  });
  const m = q.data;
  const pts = m?.points ?? [];
  const last = pts[pts.length - 1];

  // The window ends at the newest sample (or now when idle) so axes don't jump around.
  const to = last ? Math.max(last.t, Date.now() - (m?.stepSecs ?? 60) * 1000) : Date.now();
  const from = to - RANGE_SECS[range] * 1000;
  const maxGapMs = (m?.stepSecs ?? 60) * 2000;

  const vcpu = pts.map((p) => ({ t: p.t, v: p.cpuPercent / 100 }));
  const cpuDataMax = Math.max(0, ...vcpu.map((p) => p.v));
  const cpus = m?.cpus ?? 0;
  const cpuNearLimit = cpus > 0 && cpuDataMax >= cpus * 0.7;
  const cpuMax = niceMax(Math.max(1, cpuDataMax, cpuNearLimit ? cpus : 0));

  const mem = pts.map((p) => ({ t: p.t, v: p.memoryBytes }));
  const memLimit = m?.memoryLimitBytes ?? null;
  const memMax = niceMax(Math.max(...mem.map((p) => p.v), memLimit ?? 0, 1));

  const rx = pts.map((p) => ({ t: p.t, v: p.netRxBytesPerSec }));
  const tx = pts.map((p) => ({ t: p.t, v: p.netTxBytesPerSec }));
  const netMax = niceMax(Math.max(...rx.map((p) => p.v), ...tx.map((p) => p.v), 1));

  const common = { range, from, to, maxGapMs };

  return (
    <div className="flex flex-col gap-4">
      <div className="flex items-center justify-between">
        <Segmented
          label="Time range"
          value={range}
          onChange={setRange}
          options={METRICS_RANGES.map((r) => ({ value: r, label: r }))}
        />
      </div>
      {q.error ? <Alert>{q.error.message}</Alert> : null}
      {q.isLoading ? (
        <>
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
        </>
      ) : m && pts.length === 0 ? (
        <div className="rounded-xl border border-dashed border-border px-6 py-12 text-center text-sm text-muted-foreground">
          No metrics yet — they appear a minute after the service starts.
        </div>
      ) : m && last ? (
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
      ) : null}
    </div>
  );
}
