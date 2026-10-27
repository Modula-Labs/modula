import { describe, expect, it } from "vitest";
import { formatRate, formatVcpu, niceMax, splitSegments, ticks } from "./metrics";

describe("metrics helpers", () => {
  it("formats vCPU", () => {
    expect(formatVcpu(0.25)).toBe("0.25 vCPU");
    expect(formatVcpu(1)).toBe("1 vCPU");
    expect(formatVcpu(0.1234)).toBe("0.12 vCPU");
    expect(formatVcpu(12.4)).toBe("12 vCPU");
    expect(formatVcpu(null)).toBe("—");
  });
  it("formats rates", () => {
    expect(formatRate(0)).toBe("0 B/s");
    expect(formatRate(1200.5)).toBe("1.2 KB/s");
    expect(formatRate(5 * 1024 * 1024)).toBe("5.0 MB/s");
    expect(formatRate(undefined)).toBe("—");
  });
  it("picks nice maxima and ticks", () => {
    expect(niceMax(0)).toBe(1);
    expect(niceMax(0.3)).toBe(0.5);
    expect(niceMax(1.2)).toBe(2);
    expect(niceMax(730)).toBe(1000);
    expect(ticks(4, 2)).toEqual([0, 2, 4]);
  });
  it("splits series at gaps", () => {
    const p = (t: number) => ({ t, v: 1 });
    expect(splitSegments([], 120)).toEqual([]);
    expect(splitSegments([p(0), p(60), p(120)], 120)).toHaveLength(1);
    const segs = splitSegments([p(0), p(60), p(300), p(360)], 120);
    expect(segs.map((s) => s.length)).toEqual([2, 2]);
    expect(splitSegments([p(0), p(1000)], 120)).toHaveLength(2);
  });
});
