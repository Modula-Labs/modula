import { describe, expect, it } from "vitest";
import type { UpdateStatus } from "@/lib/api";
import { canUpdateNow, formatWindow, isUpdateSkipped, isUpdateSnoozed, logTail, skipUpdate, shouldOfferUpdate, snoozeUpdate, SNOOZE_MS } from "./updates";

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

describe("update snooze", () => {
  const mem = () => {
    const m = new Map<string, string>();
    return { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v) };
  };
  it("snoozes a version for 24h only", () => {
    const st = mem();
    expect(isUpdateSnoozed("1.1.0", 0, st)).toBe(false);
    snoozeUpdate("1.1.0", 1000, st);
    expect(isUpdateSnoozed("1.1.0", 1001, st)).toBe(true);
    expect(isUpdateSnoozed("1.1.0", 1000 + SNOOZE_MS + 1, st)).toBe(false);
    expect(isUpdateSnoozed("1.2.0", 1001, st)).toBe(false);
  });
  it("tolerates broken storage", () => {
    const bad = { getItem: () => { throw new Error("x"); }, setItem: () => { throw new Error("x"); } };
    expect(() => snoozeUpdate("1.1.0", 0, bad)).not.toThrow();
    expect(isUpdateSnoozed("1.1.0", 0, bad)).toBe(false);
    expect(isUpdateSnoozed("1.1.0", 0, { getItem: () => "{nope" })).toBe(false);
  });
  it("offers only when upgradable", () => {
    expect(shouldOfferUpdate(base)).toBe(true);
    expect(shouldOfferUpdate({ ...base, canUpgrade: false })).toBe(false);
    expect(shouldOfferUpdate({ ...base, updateAvailable: false })).toBe(false);
  });
});

describe("update skip", () => {
  it("skips one version only", () => {
    const m = new Map<string, string>();
    const st = { getItem: (k: string) => m.get(k) ?? null, setItem: (k: string, v: string) => void m.set(k, v) };
    expect(isUpdateSkipped("1.1.0", st)).toBe(false);
    skipUpdate("1.1.0", st);
    expect(isUpdateSkipped("1.1.0", st)).toBe(true);
    expect(isUpdateSkipped("1.2.0", st)).toBe(false);
    expect(() => skipUpdate("x", { setItem: () => { throw new Error("x"); } })).not.toThrow();
  });
});
