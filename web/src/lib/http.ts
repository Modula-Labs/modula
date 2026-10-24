import type { HttpBucket, HttpMetrics, HttpRequest } from "./api";
import type { XY } from "./metrics";

export const HTTP_METHODS = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"] as const;

export type StatusTone = "neutral" | "warn" | "danger";

/** 2xx and 3xx are neutral, 4xx amber, 5xx red; anything else (1xx, 0) is neutral. */
export function statusTone(status: number): StatusTone {
  if (status >= 500) return "danger";
  if (status >= 400) return "warn";
  return "neutral";
}

/** 0.4 -> "0.4 ms", 12.3 -> "12 ms", 1530 -> "1.53 s", 90_000 -> "1m 30s". */
export function formatDuration(ms: number | null | undefined): string {
  if (ms === null || ms === undefined || !Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1) return `${Math.round(ms * 10) / 10} ms`;
  if (ms < 1000) return `${Math.round(ms)} ms`;
  if (ms < 60_000) return `${Math.round(ms / 10) / 100} s`;
  const s = Math.round(ms / 1000);
  return `${Math.floor(s / 60)}m ${s % 60}s`;
}

/** Requests per second with precision that fits the size: 0.0125 -> "0.013/s", 4.2 -> "4.2/s", 130 -> "130/s". */
export function formatReqRate(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v) || v < 0) return "—";
  if (v === 0) return "0/s";
  if (v < 0.1) return `${Math.round(v * 1000) / 1000}/s`;
  if (v < 10) return `${Math.round(v * 10) / 10}/s`;
  return `${Math.round(v)}/s`;
}

/** 0.5 -> "0.5%", 12.34 -> "12%"; whole percent from 10 up. */
export function formatPercent(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v) || v < 0) return "—";
  if (v === 0) return "0%";
  if (v < 10) return `${Math.round(v * 10) / 10}%`;
  return `${Math.round(v)}%`;
}

/** Matches the server's `status` filter: the class a status code belongs to. */
export function statusClass(status: number): "2xx" | "3xx" | "4xx" | "5xx" | null {
  const c = Math.floor(status / 100);
  return c === 2 || c === 3 || c === 4 || c === 5 ? (`${c}xx` as "2xx") : null;
}

/** Rows to show: newest first, which is the order a person reads a request table in. */
export function newestFirst(requests: HttpRequest[]): HttpRequest[] {
  return requests.slice().reverse();
}

/** The window a histogram bar covers; the last bucket is cut off at `until`. */
export function httpBucketWindow(b: HttpBucket, bucketMs: number, until?: number): { since: number; until: number } {
  const end = b.t + bucketMs;
  return { since: b.t, until: until === undefined ? end : Math.min(end, until) };
}

export interface HttpSeries {
  /** Requests per second. */
  rate: XY[];
  /** Percent of requests that were 5xx; buckets without traffic are left out. */
  err5xx: XY[];
  /** Percent of requests that were 4xx; buckets without traffic are left out. */
  err4xx: XY[];
  p50: XY[];
  p95: XY[];
  p99: XY[];
  /** Requests over the whole range. */
  total: number;
}

/** Turns the points of an http-metrics response into chart series. Empty buckets are
 *  omitted from the ratio and latency series so the chart shows a gap, not a zero. */
export function httpSeries(m: HttpMetrics): HttpSeries {
  const out: HttpSeries = { rate: [], err5xx: [], err4xx: [], p50: [], p95: [], p99: [], total: 0 };
  const step = Math.max(m.stepSecs, 1);
  for (const p of m.points) {
    out.total += p.requests;
    out.rate.push({ t: p.t, v: p.requests / step });
    if (p.requests > 0) {
      out.err5xx.push({ t: p.t, v: (p.status5xx / p.requests) * 100 });
      out.err4xx.push({ t: p.t, v: (p.status4xx / p.requests) * 100 });
    }
    if (p.p50Ms !== null) out.p50.push({ t: p.t, v: p.p50Ms });
    if (p.p95Ms !== null) out.p95.push({ t: p.t, v: p.p95Ms });
    if (p.p99Ms !== null) out.p99.push({ t: p.t, v: p.p99Ms });
  }
  return out;
}

/** The last value of a series, for the chart's headline. */
export function lastValue(s: XY[]): number | null {
  return s.length ? s[s.length - 1]!.v : null;
}

/** Joins the latency percentiles of the newest bucket that has any, e.g. "p50 12 ms · p95 80 ms · p99 210 ms". */
export function latencyHeadline(s: Pick<HttpSeries, "p50" | "p95" | "p99">): string {
  const parts: [string, number | null][] = [
    ["p50", lastValue(s.p50)],
    ["p95", lastValue(s.p95)],
    ["p99", lastValue(s.p99)],
  ];
  return parts.map(([n, v]) => `${n} ${formatDuration(v)}`).join(" · ");
}
