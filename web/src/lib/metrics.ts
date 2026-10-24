import type { MetricsRange } from "./api";
import { formatBytes } from "./format";

export const METRICS_RANGES: MetricsRange[] = ["1h", "6h", "24h", "7d"];

/** 0.25 -> "0.25 vCPU", 2 -> "2 vCPU". Input is vCPUs, not percent. */
export function formatVcpu(v: number | null | undefined): string {
  if (v === null || v === undefined || !Number.isFinite(v) || v < 0) return "—";
  const s = v >= 10 ? String(Math.round(v)) : String(Math.round(v * 100) / 100);
  return `${s} vCPU`;
}

/** 1200.5 -> "1.2 KB/s". */
export function formatRate(bytesPerSec: number | null | undefined): string {
  const s = formatBytes(bytesPerSec);
  return s === "—" ? s : `${s}/s`;
}

export interface XY {
  t: number;
  v: number;
}

/** Splits a series into runs; a gap wider than `maxGapMs` starts a new run. */
export function splitSegments(points: XY[], maxGapMs: number): XY[][] {
  const out: XY[][] = [];
  let cur: XY[] = [];
  for (const p of points) {
    const last = cur[cur.length - 1];
    if (last && p.t - last.t > maxGapMs) {
      out.push(cur);
      cur = [];
    }
    cur.push(p);
  }
  if (cur.length) out.push(cur);
  return out;
}

/** Round the max up to 1/2/5 x 10^n so gridlines land on tidy values. */
export function niceMax(v: number): number {
  if (!(v > 0)) return 1;
  const exp = Math.floor(Math.log10(v));
  const base = 10 ** exp;
  const f = v / base;
  const n = f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10;
  return n * base;
}

/** Evenly spaced ticks from 0 to max inclusive. */
export function ticks(max: number, count = 3): number[] {
  return Array.from({ length: count + 1 }, (_, i) => (max * i) / count);
}

/** Clock label for the x axis: time of day, plus the weekday for 7d. */
export function formatAxisTime(t: number, range: MetricsRange): string {
  const d = new Date(t);
  const hm = d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  return range === "7d" ? `${d.toLocaleDateString("en-US", { weekday: "short" })} ${hm}` : hm;
}
