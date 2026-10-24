import { useQuery } from "@tanstack/react-query";
import { useState } from "react";
import { Segmented } from "@/components/ui/Tabs";
import { Alert, Skeleton } from "@/components/ui/misc";
import { api, type MetricsRange } from "@/lib/api";
import { METRICS_RANGES } from "@/lib/metrics";
import { MetricsPanels } from "./MetricsPanels";

/** CPU, memory and network of every service in one environment, summed. */
export function ProjectMetrics({ projectId, environmentId }: { projectId: string; environmentId: string }) {
  const [range, setRange] = useState<MetricsRange>("1h");
  const q = useQuery({
    queryKey: ["project-metrics", projectId, environmentId, range],
    queryFn: () => api.projectMetrics(projectId, environmentId, range),
    refetchInterval: range === "1h" || range === "6h" ? 30_000 : false,
  });
  const m = q.data;
  return (
    <div className="flex flex-col gap-6">
      <div className="flex items-center justify-between">
        <Segmented label="Time range" value={range} onChange={setRange} options={METRICS_RANGES.map((r) => ({ value: r, label: r }))} />
      </div>
      {q.error ? <Alert>{q.error.message}</Alert> : null}
      {q.isLoading ? (
        <>
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
          <Skeleton className="h-52" />
        </>
      ) : m && m.points.length === 0 ? (
        <div className="rounded-xl border border-dashed border-border px-6 py-12 text-center text-sm text-muted-foreground">
          No metrics yet — they appear a minute after a service starts.
        </div>
      ) : m ? (
        <MetricsPanels m={m} range={range} />
      ) : null}
    </div>
  );
}
