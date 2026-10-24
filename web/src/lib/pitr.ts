import { formatBytes } from "./format";

const pad = (n: number) => String(n).padStart(2, "0");

/** A `datetime-local` input value (local time, minute precision) for an ISO instant. */
export function toLocalInput(iso: string): string {
  const d = new Date(iso);
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** The UTC ISO instant for a `datetime-local` value read as local time; null when blank or invalid. */
export function fromLocalInput(value: string): string | null {
  if (!value) return null;
  const t = new Date(value).getTime();
  return Number.isNaN(t) ? null : new Date(t).toISOString();
}

/**
 * The window as `datetime-local` bounds. `min` rounds up and `max` rounds down
 * to the minute, so every value the input accepts lies inside the window.
 */
export function windowBounds(w: { from: string; to: string }): { min: string; max: string } {
  const from = new Date(w.from);
  if (from.getSeconds() || from.getMilliseconds()) from.setMinutes(from.getMinutes() + 1, 0, 0);
  const to = new Date(w.to);
  to.setSeconds(0, 0);
  return { min: toLocalInput(from.toISOString()), max: toLocalInput(to.toISOString()) };
}

/** Whether a `datetime-local` value is inside the window. */
export function inWindow(value: string, w: { from: string; to: string }): boolean {
  const iso = fromLocalInput(value);
  if (!iso) return false;
  const t = Date.parse(iso);
  return t >= Date.parse(w.from) && t <= Date.parse(w.to);
}

/** Replication lag in seconds as a short duration. */
export function formatLagSeconds(s: number | null | undefined): string {
  if (s == null) return "unknown";
  if (s < 1) return "under 1s";
  if (s < 60) return `${Math.round(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${Math.round(s % 60)}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

/** A standby's lag: time behind and bytes behind, whichever are known. */
export function formatLag(replaySeconds: number | null | undefined, bytes: number | null | undefined): string {
  const parts: string[] = [];
  if (replaySeconds != null) parts.push(formatLagSeconds(replaySeconds));
  if (bytes != null) parts.push(`${formatBytes(bytes)} behind`);
  return parts.length ? parts.join(" · ") : "unknown";
}
