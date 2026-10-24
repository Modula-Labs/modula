import { describe, expect, it } from "vitest";
import type { CronRun } from "./api";
import { cronCardLine, cronLogsWindow, cronRunDuration, isCronRunActive } from "./cron";

const run = (o: Partial<CronRun>): CronRun => ({
  id: "r1",
  deploymentId: "d1",
  trigger: "schedule",
  status: "succeeded",
  scheduledFor: null,
  timeoutSecs: 3600,
  createdAt: "2026-10-22T03:00:00Z",
  startedAt: "2026-10-22T03:00:01Z",
  finishedAt: "2026-10-22T03:02:06Z",
  exitCode: 0,
  error: null,
  ...o,
});

describe("cron runs", () => {
  it("shows the duration of a finished run and running… for an active one", () => {
    expect(cronRunDuration(run({}))).toBe("2m 5s");
    expect(cronRunDuration(run({ status: "running", finishedAt: null }))).toBe("running…");
    expect(cronRunDuration(run({ status: "queued", startedAt: null, finishedAt: null }))).toBe("—");
  });
  it("windows the logs around the run", () => {
    expect(cronLogsWindow(run({}))).toEqual({
      since: Date.parse("2026-10-22T03:00:01Z") - 5000,
      until: Date.parse("2026-10-22T03:02:06Z") + 5000,
    });
    const now = Date.parse("2026-10-22T03:01:00Z");
    expect(cronLogsWindow(run({ status: "running", finishedAt: null }), now)?.until).toBe(now + 5000);
    expect(cronLogsWindow(run({ startedAt: null, finishedAt: null }))).toBeNull();
  });
  it("words the canvas card line", () => {
    const now = Date.parse("2026-10-22T12:00:00Z");
    expect(cronCardLine("2026-10-22T14:00:00Z", now)).toBe("cron · next in 2h");
    expect(cronCardLine("2026-10-22T11:59:00Z", now)).toBe("cron · due now");
    expect(cronCardLine(null, now)).toBe("cron · not deployed");
  });
  it("knows which statuses are in flight", () => {
    expect(isCronRunActive({ status: "queued" })).toBe(true);
    expect(isCronRunActive({ status: "running" })).toBe(true);
    expect(isCronRunActive({ status: "failed" })).toBe(false);
  });
});
