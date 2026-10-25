import type { LogBucket, LogLine } from "./api";

export const MAX_LOG_LINES = 5000;
/** How long the server keeps logs and HTTP requests (its RETENTION_DAYS). */
export const RETENTION_MS = 7 * 24 * 3_600_000;

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

/** Local "YYYY-MM-DD HH:mm:ss". */
export function formatLogDateTime(t: number): string {
  const d = new Date(t);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

/** The local zone as an offset for column headers: "GMT+5:30", "GMT-8", "GMT".
 *  `offsetMinutes` is `Date#getTimezoneOffset()` (minutes *behind* UTC). */
export function timezoneLabel(offsetMinutes = new Date().getTimezoneOffset()): string {
  if (offsetMinutes === 0) return "GMT";
  const ahead = -offsetMinutes;
  const abs = Math.abs(ahead);
  const mins = abs % 60;
  return `GMT${ahead > 0 ? "+" : "-"}${Math.floor(abs / 60)}${mins ? `:${pad(mins)}` : ""}`;
}

export type LogLevel = "error" | "warn" | "info";

/** A structured (JSON object) log line, else null. */
export function parseJsonLine(text: string): Record<string, unknown> | null {
  const s = text.trim();
  if (!s.startsWith("{") || !s.endsWith("}")) return null;
  try {
    const v: unknown = JSON.parse(s);
    return v && typeof v === "object" && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
  } catch {
    return null;
  }
}

function levelOfName(v: unknown): LogLevel | null {
  // pino's numbers: 40 warn, 50 error, 60 fatal; below that is info or quieter.
  if (typeof v === "number") return v >= 50 ? "error" : v >= 40 ? "warn" : "info";
  if (typeof v !== "string") return null;
  const s = v.toLowerCase();
  if (/^(error|err|fatal|panic|crit|critical|alert|emerg|emergency|severe)$/.test(s)) return "error";
  if (/^(warn|warning)$/.test(s)) return "warn";
  if (/^(info|information|notice|debug|trace|verbose|log)$/.test(s)) return "info";
  return null;
}

// A level word near the start (after a timestamp or logger name at most),
// `level=error`, or a JS-style "TypeError: ..." line.
const TEXT_LEVEL = /^(?:\S+\s+){0,3}?[[(<]?(error|err|fatal|panic|critical|crit|warning|warn|info|debug|trace|notice)[\])>]?(?:[:|\s]|$)/i;
const KV_LEVEL = /\b(?:level|lvl|severity)=["']?([a-z]+)/i;
const JS_ERROR = /^\s*(?:Uncaught\s+)?[A-Z]\w*Error(?::|\s|$)/;

/** How bad a line is. An explicit level wins (a JSON `level`/`severity`, a
 *  leading `ERROR`/`[warn]`, `level=error`); without one, stderr counts as an error. */
export function logLevel(l: { stderr: boolean; text: string }): LogLevel {
  const json = parseJsonLine(l.text);
  if (json) {
    const named = levelOfName(json.level ?? json.severity ?? json.lvl ?? json.levelname ?? json.log_level);
    if (named) return named;
  } else {
    const m = TEXT_LEVEL.exec(l.text) ?? KV_LEVEL.exec(l.text);
    const named = m ? levelOfName(m[1]) : null;
    if (named) return named;
    if (JS_ERROR.test(l.text)) return "error";
  }
  return l.stderr ? "error" : "info";
}

/** The human part of a structured line (`msg`, `message`, …), else the line itself. */
export function logMessage(text: string, json = parseJsonLine(text)): string {
  const m = json && (json.msg ?? json.message ?? json.text ?? json.event);
  return typeof m === "string" ? m : text;
}

/** A JSON line's top-level fields as name/value rows; nested values are kept as JSON. */
export function jsonAttributes(json: Record<string, unknown>): { name: string; value: string }[] {
  return Object.entries(json).map(([name, v]) => ({
    name,
    value: typeof v === "string" ? v : v === undefined ? "" : JSON.stringify(v),
  }));
}

/** The raw line, pretty-printed when it's JSON. */
export function prettyRaw(text: string): string {
  const json = parseJsonLine(text);
  return json ? JSON.stringify(json, null, 2) : text;
}
