import { describe, expect, it } from "vitest";
import type { HttpMetrics } from "./api";
import {
  formatDuration,
  formatMs,
  formatPercent,
  formatReqRate,
  httpBucketWindow,
  httpSeries,
  latencyHeadline,
  parseHttpSearch,
  statusTextClass,
  statusTone,
} from "./http";

describe("statusTone", () => {
  it("is neutral for success and redirects, amber for 4xx, red for 5xx", () => {
    expect([200, 301, 404, 503].map(statusTone)).toEqual(["neutral", "neutral", "warn", "danger"]);
  });
});

describe("formatDuration", () => {
  it("picks a unit that fits", () => {
    expect(formatDuration(0.42)).toBe("0.4 ms");
    expect(formatDuration(12.3)).toBe("12 ms");
    expect(formatDuration(1530)).toBe("1.53 s");
    expect(formatDuration(90_000)).toBe("1m 30s");
    expect(formatDuration(null)).toBe("—");
  });
});

describe("formatReqRate / formatPercent", () => {
  it("scales precision with the value", () => {
    expect(formatReqRate(0.0125)).toBe("0.013/s");
    expect(formatReqRate(4.26)).toBe("4.3/s");
    expect(formatReqRate(130.4)).toBe("130/s");
    expect(formatPercent(0.54)).toBe("0.5%");
    expect(formatPercent(12.4)).toBe("12%");
  });
});

describe("httpBucketWindow", () => {
  it("is cut off at until", () => expect(httpBucketWindow({ t: 1000, count: 1, status4xx: 0, status5xx: 0 }, 500, 1200)).toEqual({ since: 1000, until: 1200 }));
});

describe("httpSeries", () => {
  const m: HttpMetrics = {
    range: "1h",
    stepSecs: 60,
    points: [
      { t: 0, requests: 0, status4xx: 0, status5xx: 0, p50Ms: null, p95Ms: null, p99Ms: null },
      { t: 60_000, requests: 120, status4xx: 12, status5xx: 6, p50Ms: 10, p95Ms: 80, p99Ms: 210 },
    ],
  };
  it("derives rates and ratios, leaving idle buckets out of ratios and latency", () => {
    const s = httpSeries(m);
    expect(s.rate).toEqual([
      { t: 0, v: 0 },
      { t: 60_000, v: 2 },
    ]);
    expect(s.err5xx).toEqual([{ t: 60_000, v: 5 }]);
    expect(s.err4xx).toEqual([{ t: 60_000, v: 10 }]);
    expect(s.p95).toEqual([{ t: 60_000, v: 80 }]);
    expect(s.total).toBe(120);
  });
  it("labels the newest percentiles", () => expect(latencyHeadline(httpSeries(m))).toBe("p50 10 ms · p95 80 ms · p99 210 ms"));
});

describe("statusTextClass", () => {
  it("colours each class", () => {
    expect([200, 304, 404, 502, 101].map(statusTextClass)).toEqual([
      "text-success",
      "text-info",
      "text-warning",
      "text-error",
      "text-muted-foreground",
    ]);
  });
});

describe("formatMs", () => {
  it("rounds to whole ms", () => {
    expect(formatMs(472.4)).toBe("472ms");
    expect(formatMs(0.2)).toBe("<1ms");
    expect(formatMs(12034.6)).toBe("12035ms");
  });
});

describe("parseHttpSearch", () => {
  it("reads tokens and treats other words as the path", () => {
    expect(parseHttpSearch("@path:/api/users @httpStatus:200 @method:post")).toEqual({
      path: "/api/users",
      status: "200",
      method: "POST",
      invalid: [],
    });
    expect(parseHttpSearch("  users  ")).toEqual({ path: "users", invalid: [] });
    expect(parseHttpSearch("@httpStatus:5XX health")).toEqual({ status: "5xx", path: "health", invalid: [] });
  });
  it("collects tokens it can't use", () => {
    expect(parseHttpSearch("@httpStatus:abc @method:BREW @foo:1").invalid).toEqual([
      "@httpStatus:abc",
      "@method:BREW",
      "@foo:1",
    ]);
  });
});
