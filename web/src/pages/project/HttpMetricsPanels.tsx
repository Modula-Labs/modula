import { useQuery } from "@tanstack/react-query";
import { Alert, Skeleton } from "@/components/ui/misc";
import type { HttpMetrics, MetricsRange } from "@/lib/api";
import { formatDuration, formatPercent, formatReqRate, httpSeries, lastValue, latencyHeadline } from "@/lib/http";
import { niceMax } from "@/lib/metrics";
import { MetricsChart } from "./MetricsChart";
import { RANGE_SECS } from "./MetricsPanels";

const maxOf = (...series: { v: number }[][]) => niceMax(Math.max(1e-9, ...series.flatMap((s) => s.map((p) => p.v))));

/** Request rate, error rate and latency charts from the edge's request log. `queryKey` and `fetcher`
 *  pick the scope (one service instance, or a whole environment). */
export function HttpMetricsPanels({
  queryKey,
  fetcher,
  range,
}: {
  queryKey: unknown[];
  fetcher: () => Promise<HttpMetrics>;
  range: MetricsRange;
}) {
  const q = useQuery({
    queryKey: ["http-metrics", ...queryKey, range],
    queryFn: fetcher,
    refetchInterval: range === "1h" || range === "6h" ? 30_000 : false,
  });
  const m = q.data;
  if (q.error) return <Alert>{q.error.message}</Alert>;
  if (q.isLoading) return <Skeleton className="h-52" />;
  if (!m) return null;

  const s = httpSeries(m);
  if (s.total === 0) {
    return (
      <div className="rounded-xl border border-dashed border-border px-6 py-8 text-center text-sm text-muted-foreground">
        No HTTP requests in this time range.
      </div>
    );
  }

  const last = m.points[m.points.length - 1]!;
  // The window ends where the newest bucket does, so the axes line up with the other charts' range.
  const to = last.t + m.stepSecs * 1000;
  const from = to - RANGE_SECS[range] * 1000;
  const common = { range, from, to, maxGapMs: m.stepSecs * 1500 };

  return (
    <>
      <MetricsChart
        {...common}
        title="Requests"
        latest={formatReqRate(lastValue(s.rate))}
        series={[{ name: "Requests", color: "var(--chart-1)", data: s.rate }]}
        max={maxOf(s.rate)}
        yLabel={formatReqRate}
        valueLabel={formatReqRate}
      />
      <MetricsChart
        {...common}
        title="Errors"
        latest={`5xx ${formatPercent(lastValue(s.err5xx))} · 4xx ${formatPercent(lastValue(s.err4xx))}`}
        series={[
          { name: "5xx", color: "var(--destructive)", data: s.err5xx },
          { name: "4xx", color: "var(--warning)", data: s.err4xx },
        ]}
        max={Math.max(1, maxOf(s.err5xx, s.err4xx))}
        yLabel={formatPercent}
        valueLabel={formatPercent}
      />
      <MetricsChart
        {...common}
        title="Latency"
        latest={latencyHeadline(s)}
        series={[
          { name: "p50", color: "var(--chart-4)", data: s.p50 },
          { name: "p95", color: "var(--chart-2)", data: s.p95 },
          { name: "p99", color: "var(--chart-1)", data: s.p99 },
        ]}
        max={maxOf(s.p50, s.p95, s.p99)}
        yLabel={formatDuration}
        valueLabel={formatDuration}
      />
    </>
  );
}
