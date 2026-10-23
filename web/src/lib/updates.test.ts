import { describe, expect, it } from "vitest";
import type { UpdateStatus } from "@/lib/api";
import { canUpdateNow, formatWindow, logTail } from "./updates";

const base: UpdateStatus = {
  currentVersion: "1.0.0", latestVersion: "1.1.0", updateAvailable: true, releaseNotesUrl: null, checkedAt: null, checkError: null,
  mode: "notify", channel: "stable", window: { days: [1], time: "03:00", timezone: "UTC" }, nextWindow: null, canUpgrade: true, lastRun: null, servers: [],
};
const run = (status: NonNullable<UpdateStatus["lastRun"]>["status"]) => ({
  id: "r", fromVersion: "1.0.0", toVersion: "1.1.0", status, trigger: "manual" as const, requestedAt: "", finishedAt: null, log: null,
});

describe("updates helpers", () => {
  it("formats windows", () => {
    expect(formatWindow({ days: [4, 1], time: "03:00", timezone: "Europe/Berlin" })).toBe("Mon, Thu at 03:00 (Europe/Berlin)");
    expect(formatWindow({ days: [0, 1, 2, 3, 4, 5, 6], time: "02:30", timezone: "UTC" })).toBe("Every day at 02:30 (UTC)");
    expect(formatWindow({ days: [], time: "02:30", timezone: "UTC" })).toContain("No days");
  });
  it("gates Update now", () => {
    expect(canUpdateNow(base)).toBe(true);
    expect(canUpdateNow(undefined)).toBe(false);
    expect(canUpdateNow({ ...base, updateAvailable: false })).toBe(false);
    expect(canUpdateNow({ ...base, canUpgrade: false })).toBe(false);
    expect(canUpdateNow({ ...base, lastRun: run("running") })).toBe(false);
    expect(canUpdateNow({ ...base, lastRun: run("requested") })).toBe(false);
    expect(canUpdateNow({ ...base, lastRun: run("failed") })).toBe(true);
  });
  it("tails logs", () => {
    expect(logTail(null)).toBe("");
    expect(logTail("a\nb\nc\n", 2)).toBe("b\nc");
  });
});
