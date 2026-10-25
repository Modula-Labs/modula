import { describe, expect, it } from "vitest";
import type { HttpMetricsPoint } from "./api";
import {
  bucketStarts,
  errorBuckets,
  errorSeverity,
  formatDownMs,
  formatErrorShare,
  formatUptime,
  uptimeLabel,
} from "./uptime";

const pt = (t: number, requests: number, status5xx: number): HttpMetricsPoint => ({
  t,
  requests,
  status4xx: 0,
  status5xx,
  p50Ms: null,
  p95Ms: null,
  p99Ms: null,
});

describe("uptime strip helpers", () => {
  it("lays out buckets ending at now", () => {
    const now = Date.UTC(2026, 9, 25, 12, 47, 10);
    const s = bucketStarts(now, "24h");
    expect(s).toHaveLength(48);
    expect(s[47]).toBe(Date.UTC(2026, 9, 25, 12, 30));
    expect(s[46]).toBe(Date.UTC(2026, 9, 25, 12, 0));
    expect(bucketStarts(now, "1h")).toHaveLength(60);
    expect(bucketStarts(now, "7d")).toHaveLength(84);
  });

  it("formats uptime without rounding up to 100%", () => {
    expect(formatUptime(1)).toBe("100%");
    expect(formatUptime(0.99999)).toBe("99.9%");
    expect(formatUptime(0.9921)).toBe("99.2%");
    expect(formatUptime(null)).toBe("—");
  });

  it("describes a bucket", () => {
    expect(formatDownMs(240_000)).toBe("4m");
    expect(formatDownMs(72 * 60_000)).toBe("1h 12m");
    expect(formatDownMs(49 * 3_600_000)).toBe("2d 1h");
    expect(uptimeLabel({ state: "down", downMs: 240_000, reason: "crashed" })).toBe("Down 4m (crashed)");
    expect(uptimeLabel({ state: "up", downMs: 0, reason: null })).toBe("Up");
    expect(uptimeLabel({ state: "none", downMs: 0, reason: null })).toBe("No data");
  });

  it("regroups HTTP points into the strip's buckets", () => {
    const starts = [0, 1_800_000];
    const b = errorBuckets([pt(0, 10, 1), pt(900_000, 5, 0), pt(1_800_000, 3, 3), pt(9e9, 1, 1)], starts, "24h");
    expect(b).toEqual([
      { t: 0, requests: 15, errors: 1 },
      { t: 1_800_000, requests: 3, errors: 3 },
    ]);
  });

  it("scales 5xx severity by share, damped at low traffic", () => {
    expect(errorSeverity(0, 0)).toBeNull();
    expect(errorSeverity(100, 0)).toBe(0);
    expect(errorSeverity(1000, 200)).toBe(1);
    // 1 of 2 requests: 50% share, but a single error stays well short of red.
    expect(errorSeverity(2, 1)!).toBeLessThan(0.5);
    // A trace of errors is still visibly not green.
    expect(errorSeverity(100_000, 1)).toBe(0.15);
    expect(formatErrorShare(1000, 3)).toBe("0.3%");
    expect(formatErrorShare(100_000, 1)).toBe("<0.1%");
    expect(formatErrorShare(10, 0)).toBe("0%");
  });
});
