import { useQuery } from "@tanstack/react-query";
import { useState, type ReactNode } from "react";
import type { HttpMetrics, MetricsRange, Uptime } from "@/lib/api";
import {
  bucketSpanLabel,
  bucketStarts,
  errorBuckets,
  errorSeverity,
  formatErrorShare,
  formatUptime,
  severityColor,
  uptimeLabel,
} from "@/lib/uptime";

export interface StripBar {
  t: number;
  /** null = no data (a dim bar). */
  color: string | null;
  tip: ReactNode;
}

const DIM = "color-mix(in oklch, var(--muted-foreground) 14%, transparent)";

/** One row of thin square bars filling the width, like a status page, with a tooltip per bar. */
export function StatusStrip({ bars, label }: { bars: StripBar[]; label: string }) {
  const [hover, setHover] = useState<number | null>(null);
  const hb = hover === null ? undefined : bars[hover];
  const pos = hover === null ? 0 : (hover + 0.5) / bars.length;
  return (
    <div className="relative" onPointerLeave={() => setHover(null)}>
      <div role="img" aria-label={label} className="flex h-4 gap-[3px]">
        {bars.map((b, i) => (
          <div
            key={b.t}
            onPointerEnter={() => setHover(i)}
            className="min-w-px flex-1 transition-opacity"
            style={{ background: b.color ?? DIM, opacity: hover === null || hover === i ? 1 : 0.7 }}
          />
        ))}
      </div>
      {hb ? (
        <div
          className="pointer-events-none absolute bottom-full z-10 mb-1.5 whitespace-nowrap rounded-lg border border-border bg-popover px-2 py-1 text-xs shadow-raise"
          style={pos > 0.6 ? { right: `${(1 - pos) * 100}%` } : { left: `${pos * 100}%` }}
        >
          {hb.tip}
        </div>
      ) : null}
    </div>
  );
}

/** The uptime strip and, under it, the HTTP 5xx strip, with their one-line caption after
 *  `children` (the range picker). `scope` keys the queries; the HTTP query shares its cache
 *  with the HTTP charts below. */
export function HealthStrips({
  range,
  scope,
  uptime,
  http,
  children,
}: {
  range: MetricsRange;
  scope: unknown[];
  uptime: () => Promise<Uptime>;
  http: () => Promise<HttpMetrics>;
  children: ReactNode;
}) {
  const live = range === "1h" || range === "6h" ? 30_000 : false;
  const uq = useQuery({ queryKey: ["uptime", ...scope, range], queryFn: uptime, refetchInterval: live });
  const hq = useQuery({ queryKey: ["http-metrics", ...scope, range], queryFn: http, refetchInterval: live });
  const u = uq.data;

  const starts = u?.buckets.map((b) => b.t) ?? bucketStarts(Date.now(), range);
  const upBars: StripBar[] = u
    ? u.buckets.map((b) => ({
        t: b.t,
        color: b.state === "up" ? "color-mix(in oklch, var(--muted-foreground) 55%, transparent)" : b.state === "down" ? "var(--destructive)" : null,
        tip: (
          <>
            <div className="text-muted-foreground">{bucketSpanLabel(b.t, range)}</div>
            <div className={b.state === "down" ? "text-error" : undefined}>{uptimeLabel(b)}</div>
          </>
        ),
      }))
    : starts.map((t) => ({ t, color: null, tip: <div className="text-muted-foreground">{bucketSpanLabel(t, range)}</div> }));

  const eb = errorBuckets(hq.data?.points ?? [], starts, range);
  const totals = eb.reduce((a, b) => ({ requests: a.requests + b.requests, errors: a.errors + b.errors }), { requests: 0, errors: 0 });
  const errBars: StripBar[] = eb.map((b) => {
    const sev = errorSeverity(b.requests, b.errors);
    return {
      t: b.t,
      color: sev === null ? null : severityColor(sev),
      tip: (
        <>
          <div className="text-muted-foreground">{bucketSpanLabel(b.t, range)}</div>
          {b.requests === 0 ? (
            <div>No requests</div>
          ) : (
            <div className="tabular-nums">
              {b.requests} {b.requests === 1 ? "request" : "requests"} ·{" "}
              <span className={b.errors > 0 ? "text-error" : undefined}>
                {b.errors} 5xx ({formatErrorShare(b.requests, b.errors)})
              </span>
            </div>
          )}
        </>
      ),
    };
  });

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        {children}
        <span className="text-xs tabular-nums text-muted-foreground">
          {u ? (u.uptime === null ? "Uptime —" : `Uptime ${formatUptime(u.uptime)}`) : uq.error ? "Uptime unavailable" : "Uptime …"}
          {" · "}
          {totals.requests === 0
            ? "no requests"
            : `5xx ${formatErrorShare(totals.requests, totals.errors)} · ${totals.errors} ${totals.errors === 1 ? "error" : "errors"}`}
          {` · last ${range}`}
        </span>
      </div>
      <div className="flex flex-col gap-1.5">
        <StatusStrip bars={upBars} label={`Uptime over the last ${range}: grey up, red down, dim no data`} />
        <StatusStrip bars={errBars} label={`HTTP 5xx errors over the last ${range}: green none, redder as errors rise`} />
      </div>
    </div>
  );
}
