import { describe, expect, it } from "vitest";
import { describeAgent, formatCount, formatDuration, formatMs, formatRatio, formatUtc, relativeTime, relativeUntil } from "./format";

describe("format", () => {
  it("describes user agents", () => {
    expect(describeAgent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 Chrome/131 Safari/537.36")).toBe("Chrome on macOS");
    expect(describeAgent("curl/8.7.1")).toBe("CLI");
    expect(describeAgent(null)).toBe("Unknown device");
  });
  it("formats relative time", () => {
    const now = Date.parse("2026-10-22T12:00:00Z");
    expect(relativeTime("2026-10-22T11:59:50Z", now)).toBe("just now");
    expect(relativeTime("2026-10-22T11:00:00Z", now)).toBe("1h ago");
    expect(relativeTime("2026-10-20T12:00:00Z", now)).toBe("2d ago");
  });
  it("formats counts, ratios and durations", () => {
    expect(formatCount(1234567)).toBe("1,234,567");
    expect(formatCount(null)).toBe("—");
    expect(formatRatio(0.5)).toBe("50%");
    expect(formatRatio(0.9876)).toBe("98.8%");
    expect(formatMs(0.5)).toBe("0.50 ms");
    expect(formatMs(250)).toBe("250 ms");
    expect(formatMs(1500)).toBe("1.5 s");
  });
  it("formats future time", () => {
    const now = Date.parse("2026-10-22T12:00:00Z");
    expect(relativeUntil("2026-10-22T12:00:20Z", now)).toBe("due now");
    expect(relativeUntil("2026-10-22T11:00:00Z", now)).toBe("due now");
    expect(relativeUntil("2026-10-22T12:15:00Z", now)).toBe("in 15m");
    expect(relativeUntil("2026-10-22T14:00:00Z", now)).toBe("in 2h");
    expect(relativeUntil("2026-10-25T12:00:00Z", now)).toBe("in 3d");
  });
  it("formats elapsed durations", () => {
    expect(formatDuration(4000)).toBe("4s");
    expect(formatDuration(125_000)).toBe("2m 5s");
    expect(formatDuration(120_000)).toBe("2m");
    expect(formatDuration(3_700_000)).toBe("1h 1m");
    expect(formatDuration(-5)).toBe("0s");
  });
  it("formats UTC times", () => {
    expect(formatUtc("2026-10-25T03:00:00Z")).toBe("Sun, Oct 25 03:00 UTC");
  });
});
