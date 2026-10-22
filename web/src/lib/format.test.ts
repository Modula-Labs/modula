import { describe, expect, it } from "vitest";
import { describeAgent, relativeTime } from "./format";

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
});
