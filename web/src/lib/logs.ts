import type { LogBucket, LogLine } from "./api";

export const MAX_LOG_LINES = 5000;

export const LOG_RANGES = [
  { id: "15m", label: "Last 15 min", ms: 15 * 60_000 },
  { id: "1h", label: "Last hour", ms: 3_600_000 },
  { id: "6h", label: "Last 6 hours", ms: 6 * 3_600_000 },
  { id: "24h", label: "Last 24 hours", ms: 24 * 3_600_000 },
  { id: "7d", label: "Last 7 days", ms: 7 * 24 * 3_600_000 },
] as const;
export type LogRangeId = (typeof LOG_RANGES)[number]["id"];

export const logRangeMs = (id: LogRangeId) => LOG_RANGES.find((r) => r.id === id)!.ms;

/** Appends `next` to `prev` and keeps the newest `max` lines. */
export function appendLines(prev: LogLine[], next: LogLine[], max = MAX_LOG_LINES): LogLine[] {
  if (next.length === 0) return prev;
  const all = prev.concat(next);
  return all.length > max ? all.slice(all.length - max) : all;
}

/** Where a live poll continues from: just after the newest line, else where the last poll ended. */
export function nextSince(lines: LogLine[], fallback: number): number {
  const last = lines[lines.length - 1];
  return last ? Math.max(last.t + 1, fallback) : fallback;
}

const pad = (n: number, w = 2) => String(n).padStart(w, "0");

/** Local "HH:mm:ss.SSS". */
export function formatLogTime(t: number): string {
  const d = new Date(t);
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`;
}

/** One line of the downloaded file: `ISO-time service stream line`. */
export function formatLogFileLine(l: LogLine): string {
  return `${new Date(l.t).toISOString()} ${l.serviceName} ${l.stream} ${l.line}`;
}

/** Splits `text` around case-insensitive matches of `q` for highlighting. */
export function highlightParts(text: string, q: string): { text: string; match: boolean }[] {
  const needle = q.trim().toLowerCase();
  if (!needle) return [{ text, match: false }];
  const hay = text.toLowerCase();
  const out: { text: string; match: boolean }[] = [];
  let at = 0;
  for (let i = hay.indexOf(needle); i !== -1; i = hay.indexOf(needle, at)) {
    if (i > at) out.push({ text: text.slice(at, i), match: false });
    out.push({ text: text.slice(i, i + needle.length), match: true });
    at = i + needle.length;
  }
  if (at < text.length) out.push({ text: text.slice(at), match: false });
  return out.length ? out : [{ text, match: false }];
}

/** The window a histogram bar covers; the last bucket is cut off at `until`. */
export function bucketWindow(b: LogBucket, bucketMs: number, until?: number): { since: number; until: number } {
  const end = b.t + bucketMs;
  return { since: b.t, until: until === undefined ? end : Math.min(end, until) };
}

/** Axis/tooltip label for a point in a window `spanMs` wide: "HH:mm", with the day once it spans over a day. */
export function formatWindowTime(t: number, spanMs: number): string {
  const d = new Date(t);
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  return spanMs > 24 * 3_600_000 ? `${d.toLocaleDateString("en-US", { month: "short", day: "numeric" })} ${hm}` : hm;
}
