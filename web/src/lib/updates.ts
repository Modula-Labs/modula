import type { UpdateStatus, UpdateWindow } from "@/lib/api";

const DAY_NAMES = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/** "Every day at 03:00 (Europe/Berlin)" / "Mon, Thu at 03:00 (UTC)". */
export function formatWindow(w: UpdateWindow): string {
  const days = [...new Set(w.days)].filter((d) => d >= 0 && d <= 6).sort((a, b) => a - b);
  const label = days.length === 0 ? "No days selected" : days.length === 7 ? "Every day" : days.map((d) => DAY_NAMES[d]).join(", ");
  return `${label} at ${w.time} (${w.timezone})`;
}

export function isRunActive(s: Pick<UpdateStatus, "lastRun"> | undefined): boolean {
  const st = s?.lastRun?.status;
  return st === "requested" || st === "running";
}

/** "Update now" is allowed only when there is something to install, the instance can self-upgrade, and no run is in flight. */
export function canUpdateNow(s: UpdateStatus | undefined): boolean {
  return !!s && s.updateAvailable && s.canUpgrade && !isRunActive(s);
}

/** Last `n` lines of a log. */
export function logTail(log: string | null, n = 60): string {
  if (!log) return "";
  const lines = log.trimEnd().split("\n");
  return lines.slice(-n).join("\n");
}
