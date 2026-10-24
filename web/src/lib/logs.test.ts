import { describe, expect, it } from "vitest";
import type { LogLine } from "./api";
import { appendLines, bucketWindow, formatLogFileLine, formatWindowTime, highlightParts, nextSince } from "./logs";

const line = (t: number, text = "x", stream: LogLine["stream"] = "stdout"): LogLine => ({
  t,
  serviceId: "s1",
  serviceName: "api",
  deploymentId: "d1",
  stream,
  line: text,
});

describe("appendLines", () => {
  it("appends and keeps the newest lines", () => {
    const out = appendLines([line(1), line(2), line(3)], [line(4), line(5)], 4);
    expect(out.map((l) => l.t)).toEqual([2, 3, 4, 5]);
  });
  it("returns the same array when there is nothing new", () => {
    const prev = [line(1)];
    expect(appendLines(prev, [])).toBe(prev);
  });
});

describe("nextSince", () => {
  it("continues just after the newest line", () => expect(nextSince([line(10), line(20)], 5)).toBe(21));
  it("falls back when there are no lines", () => expect(nextSince([], 5)).toBe(5));
});

describe("highlightParts", () => {
  it("marks every case-insensitive match", () => {
    expect(highlightParts("Error: error found", "ERROR")).toEqual([
      { text: "Error", match: true },
      { text: ": ", match: false },
      { text: "error", match: true },
      { text: " found", match: false },
    ]);
  });
  it("returns the text as is without a query", () => expect(highlightParts("abc", " ")).toEqual([{ text: "abc", match: false }]));
});

describe("formatLogFileLine", () => {
  it("is ISO time, service, stream, line", () =>
    expect(formatLogFileLine(line(0, "hello", "stderr"))).toBe("1970-01-01T00:00:00.000Z api stderr hello"));
});

describe("bucketWindow", () => {
  it("covers the bucket", () => expect(bucketWindow({ t: 1000, count: 1, errors: 0 }, 500)).toEqual({ since: 1000, until: 1500 }));
  it("is cut off at until", () => expect(bucketWindow({ t: 1000, count: 1, errors: 0 }, 500, 1200)).toEqual({ since: 1000, until: 1200 }));
});

describe("formatWindowTime", () => {
  it("is just the clock within a day", () => expect(formatWindowTime(new Date(2026, 0, 5, 9, 7).getTime(), 3_600_000)).toBe("09:07"));
  it("adds the day for longer windows", () =>
    expect(formatWindowTime(new Date(2026, 0, 5, 9, 7).getTime(), 7 * 24 * 3_600_000)).toBe("Jan 5 09:07"));
});
