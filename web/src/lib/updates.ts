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

export const SNOOZE_KEY = "modula.updates.snooze";
export const SNOOZE_MS = 24 * 60 * 60 * 1000;

/** Hide the update prompt for `version` for 24h. Storage failures are ignored. */
export function snoozeUpdate(version: string, now = Date.now(), storage?: Pick<Storage, "setItem">): void {
  try {
    (storage ?? localStorage).setItem(SNOOZE_KEY, JSON.stringify({ version, until: now + SNOOZE_MS }));
  } catch {
    /* storage unavailable */
  }
}

/** True while `version` is snoozed. A newer version is never snoozed by an older snooze. */
export function isUpdateSnoozed(version: string, now = Date.now(), storage?: Pick<Storage, "getItem">): boolean {
  try {
    const raw = (storage ?? localStorage).getItem(SNOOZE_KEY);
    if (!raw) return false;
    const v = JSON.parse(raw) as { version?: unknown; until?: unknown };
    return v.version === version && typeof v.until === "number" && v.until > now;
  } catch {
    return false;
  }
}

/** Whether the shell should offer the update prompt (before snooze/active-run handling). */
export function shouldOfferUpdate(s: UpdateStatus | undefined): boolean {
  return !!s && s.updateAvailable && !!s.latestVersion && s.canUpgrade;
}

export const SKIP_KEY = "modula.updates.skip";

/** Never prompt for `version` again (a newer version prompts again). */
export function skipUpdate(version: string, storage?: Pick<Storage, "setItem">): void {
  try {
    (storage ?? localStorage).setItem(SKIP_KEY, version);
  } catch {
    /* storage unavailable */
  }
}

export function isUpdateSkipped(version: string, storage?: Pick<Storage, "getItem">): boolean {
  try {
    return (storage ?? localStorage).getItem(SKIP_KEY) === version;
  } catch {
    return false;
  }
}
