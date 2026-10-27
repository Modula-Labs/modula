import { describe, expect, it } from "vitest";
import type { LogLine } from "./api";
import {
  appendLines,
  bucketWindow,
  formatLogDateTime,
  formatLogFileLine,
  formatWindowTime,
  highlightParts,
  jsonAttributes,
  logLevel,
  logMessage,
  nextSince,
  parseJsonLine,
  prettyRaw,
  timezoneLabel,
} from "./logs";

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

describe("formatLogDateTime", () => {
  it("is local YYYY-MM-DD HH:mm:ss", () => {
    const t = new Date(2026, 0, 5, 7, 8, 9, 123).getTime();
    expect(formatLogDateTime(t)).toBe("2026-01-05 07:08:09");
  });
});

describe("timezoneLabel", () => {
  it("flips getTimezoneOffset's sign and shows minutes only when there are some", () => {
    expect(timezoneLabel(-330)).toBe("GMT+5:30");
    expect(timezoneLabel(480)).toBe("GMT-8");
    expect(timezoneLabel(0)).toBe("GMT");
    expect(timezoneLabel(-345)).toBe("GMT+5:45");
  });
});

describe("logLevel", () => {
  const lvl = (text: string, stderr = false) => logLevel({ text, stderr });
  it("reads a leading level word or level=", () => {
    expect(lvl("2026-10-25T10:00:00Z ERROR db down")).toBe("error");
    expect(lvl("[warn] slow query")).toBe("warn");
    expect(lvl("FATAL: out of memory")).toBe("error");
    expect(lvl('time=1 level=warning msg="x"')).toBe("warn");
    expect(lvl("TypeError: x is undefined")).toBe("error");
  });
  it("doesn't mistake a mention for a level", () => {
    expect(lvl("0 errors found in 12 files")).toBe("info");
    expect(lvl("GET /health 200")).toBe("info");
  });
  it("reads JSON levels, pino numbers included", () => {
    expect(lvl('{"level":"error","msg":"boom"}')).toBe("error");
    expect(lvl('{"level":50,"msg":"boom"}')).toBe("error");
    expect(lvl('{"level":40,"msg":"hm"}')).toBe("warn");
    expect(lvl('{"severity":"WARNING"}')).toBe("warn");
  });
  it("counts stderr as an error unless the line says otherwise", () => {
    expect(lvl("something happened", true)).toBe("error");
    expect(lvl("INFO started", true)).toBe("info");
    expect(lvl('{"level":30,"msg":"ok"}', true)).toBe("info");
  });
});

describe("structured lines", () => {
  it("parses objects only", () => {
    expect(parseJsonLine('{"a":1}')).toEqual({ a: 1 });
    expect(parseJsonLine("[1]")).toBeNull();
    expect(parseJsonLine("{nope}")).toBeNull();
  });
  it("finds the message and flattens top-level fields", () => {
    const j = parseJsonLine('{"level":"info","msg":"hi","req":{"id":7}}')!;
    expect(logMessage("raw", j)).toBe("hi");
    expect(logMessage("plain text")).toBe("plain text");
    expect(jsonAttributes(j)).toEqual([
      { name: "level", value: "info" },
      { name: "msg", value: "hi" },
      { name: "req", value: '{"id":7}' },
    ]);
  });
  it("pretty-prints JSON raw data", () => {
    expect(prettyRaw('{"a":1}')).toBe('{\n  "a": 1\n}');
    expect(prettyRaw("plain")).toBe("plain");
  });
});
