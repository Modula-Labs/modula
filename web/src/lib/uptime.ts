import type { HttpMetricsPoint, MetricsRange, UptimeBucket } from "./api";
import { formatWindowTime } from "./logs";

/** Bucket width of the uptime and 5xx strips; matches the server's `Range::uptime_step`. */
export const STRIP_STEP_SECS: Record<MetricsRange, number> = { "1h": 60, "6h": 300, "24h": 1800, "7d": 7200 };
export const STRIP_SPAN_SECS: Record<MetricsRange, number> = { "1h": 3600, "6h": 6 * 3600, "24h": 24 * 3600, "7d": 7 * 24 * 3600 };

/** Bucket starts (unix ms, oldest first) of a strip whose last bucket holds `nowMs`. */
export function bucketStarts(nowMs: number, range: MetricsRange): number[] {
  const step = STRIP_STEP_SECS[range] * 1000;
  const n = STRIP_SPAN_SECS[range] / STRIP_STEP_SECS[range];
  const last = Math.floor(nowMs / step) * step;
  return Array.from({ length: n }, (_, i) => last - (n - 1 - i) * step);
}

/** 0.9921 -> "99.2%"; rounded down so a bad minute never shows as 100%. */
export function formatUptime(ratio: number | null | undefined): string {
  if (ratio === null || ratio === undefined || !Number.isFinite(ratio)) return "—";
  if (ratio >= 1) return "100%";
  return `${(Math.floor(ratio * 1000) / 10).toFixed(1)}%`;
}

/** 240000 -> "4m", 4320000 -> "1h 12m", 2 days -> "2d". */
export function formatDownMs(ms: number): string {
  const m = Math.max(1, Math.round(ms / 60_000));
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
  const d = Math.floor(h / 24);
  return h % 24 ? `${d}d ${h % 24}h` : `${d}d`;
}

/** "Up", "Down 4m (crashed)", "No data". */
export function uptimeLabel(b: Pick<UptimeBucket, "state" | "downMs" | "reason">): string {
  if (b.state === "up") return "Up";
  if (b.state === "none") return "No data";
  return `Down ${formatDownMs(b.downMs)}${b.reason ? ` (${b.reason})` : ""}`;
}

/** "14:00–14:30" (with the date for the 7-day strip). */
export function bucketSpanLabel(t: number, range: MetricsRange): string {
  const span = STRIP_SPAN_SECS[range] * 1000;
  return `${formatWindowTime(t, span)}–${formatWindowTime(t + STRIP_STEP_SECS[range] * 1000, span)}`;
}

export interface ErrorBucket {
  t: number;
  requests: number;
  errors: number;
}

/** Regroups the HTTP chart points (whose buckets divide the strip's) into the strip's buckets. */
export function errorBuckets(points: HttpMetricsPoint[], starts: number[], range: MetricsRange): ErrorBucket[] {
  const step = STRIP_STEP_SECS[range] * 1000;
  const out = starts.map((t) => ({ t, requests: 0, errors: 0 }));
  if (!out.length) return out;
  const first = starts[0]!;
  for (const p of points) {
    const i = Math.floor((p.t - first) / step);
    const b = out[i];
    if (!b) continue;
    b.requests += p.requests;
    b.errors += p.status5xx;
  }
  return out;
}

/** How red a 5xx bar is, 0 (none) to 1; null without requests. The share of 5xx, but a few
 *  errors at low traffic (1 of 2 requests) stay orange rather than turning fully red:
 *  10% of requests or 10 errors, whichever is reached last, is fully red. */
export function errorSeverity(requests: number, errors: number): number | null {
  if (requests <= 0) return null;
  if (errors <= 0) return 0;
  const byShare = Math.min(1, errors / requests / 0.1);
  const byCount = Math.min(1, errors / 10);
  return Math.max(0.15, Math.sqrt(Math.min(byShare, byCount)));
}

/** Green to red through the theme's own colors. */
export function severityColor(sev: number): string {
  return `color-mix(in oklch, var(--destructive) ${Math.round(sev * 100)}%, var(--success))`;
}

/** 5xx share as "0.3%" ("0%" for none, "<0.1%" for a trace). */
export function formatErrorShare(requests: number, errors: number): string {
  if (requests <= 0 || errors <= 0) return "0%";
  const pct = (errors / requests) * 100;
  if (pct < 0.1) return "<0.1%";
  return pct < 10 ? `${Math.round(pct * 10) / 10}%` : `${Math.round(pct)}%`;
}
