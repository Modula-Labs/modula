import type { Tone } from "@/components/ui/misc";
import type { CronRun, CronRunStatus } from "./api";
import { formatDuration, relativeUntil } from "./format";

/** How a cron run status reads in badges. */
export const cronRunBadge: Record<CronRunStatus, { label: string; tone: Tone }> = {
  queued: { label: "Queued", tone: "neutral" },
  running: { label: "Running", tone: "strong" },
  succeeded: { label: "Succeeded", tone: "success" },
  failed: { label: "Failed", tone: "danger" },
  timed_out: { label: "Timed out", tone: "danger" },
  skipped: { label: "Skipped", tone: "neutral" },
};

export const isCronRunActive = (r: Pick<CronRun, "status">) => r.status === "queued" || r.status === "running";

/** Run time: "running…" while in flight, "—" if it never started. */
export function cronRunDuration(r: CronRun): string {
  if (!r.startedAt) return "—";
  if (!r.finishedAt) return r.status === "running" ? "running…" : "—";
  return formatDuration(Date.parse(r.finishedAt) - Date.parse(r.startedAt));
}

/** The canvas card subtitle of a cron service: "cron · next in 2h". */
export function cronCardLine(nextAt: string | null, now = Date.now()): string {
  if (!nextAt) return "cron · not deployed";
  const until = relativeUntil(nextAt, now);
  return `cron · ${until === "due now" ? until : `next ${until}`}`;
}

/** Slack around a run's window so its first and last lines are included. */
const LOG_MARGIN_MS = 5_000;

/** The Logs page window for one run: from just before it started until just after it ended (or now while running). */
export function cronLogsWindow(r: CronRun, now = Date.now()): { since: number; until: number } | null {
  if (!r.startedAt) return null;
  const end = r.finishedAt ? Date.parse(r.finishedAt) : now;
  return { since: Date.parse(r.startedAt) - LOG_MARGIN_MS, until: end + LOG_MARGIN_MS };
}
