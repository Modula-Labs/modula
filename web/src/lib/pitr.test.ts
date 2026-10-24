import { describe, expect, it } from "vitest";
import { formatLag, formatLagSeconds, fromLocalInput, inWindow, toLocalInput, windowBounds } from "./pitr";

describe("pitr", () => {
  it("round-trips local datetime values through UTC ISO", () => {
    const iso = "2026-10-22T12:34:00.000Z";
    expect(fromLocalInput(toLocalInput(iso))).toBe(iso);
    expect(fromLocalInput("")).toBeNull();
    expect(fromLocalInput("nope")).toBeNull();
  });
  it("keeps the window bounds inside the window", () => {
    const w = { from: "2026-10-22T10:00:30Z", to: "2026-10-22T12:00:45Z" };
    const { min, max } = windowBounds(w);
    expect(fromLocalInput(min)).toBe("2026-10-22T10:01:00.000Z");
    expect(fromLocalInput(max)).toBe("2026-10-22T12:00:00.000Z");
    expect(inWindow(min, w)).toBe(true);
    expect(inWindow(max, w)).toBe(true);
    expect(inWindow(toLocalInput("2026-10-22T09:00:00Z"), w)).toBe(false);
    expect(inWindow(toLocalInput("2026-10-22T13:00:00Z"), w)).toBe(false);
    expect(inWindow("", w)).toBe(false);
  });
  it("formats lag", () => {
    expect(formatLagSeconds(null)).toBe("unknown");
    expect(formatLagSeconds(0.2)).toBe("under 1s");
    expect(formatLagSeconds(42)).toBe("42s");
    expect(formatLagSeconds(125)).toBe("2m 5s");
    expect(formatLagSeconds(3900)).toBe("1h 5m");
    expect(formatLag(null, null)).toBe("unknown");
    expect(formatLag(3, 2048)).toBe(`3s · ${"2.0 KB"} behind`);
  });
});
