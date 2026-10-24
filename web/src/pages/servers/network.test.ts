import { describe, expect, it } from "vitest";
import { formatLatency, linkState } from "./NetworkPanel";
import { replicasError } from "../project/SettingsTab";

const link = (over: Partial<Parameters<typeof linkState>[0]> = {}) => ({
  from: "a",
  to: "b",
  path: "private" as const,
  endpoint: "10.0.0.3:51820",
  latencyMs: 0.42,
  lastHandshake: null,
  ...over,
});

describe("network tab", () => {
  it("tells live tunnels from stale ones", () => {
    const now = 1_000_000;
    expect(linkState(link({ lastHandshake: now - 30 }), now)).toBe("up");
    expect(linkState(link({ lastHandshake: now - 600 }), now)).toBe("down");
    expect(linkState(link(), now)).toBe("down");
    expect(linkState(link({ path: "none", endpoint: null }), now)).toBe("waiting");
  });

  it("formats latency", () => {
    expect(formatLatency(null)).toBe("—");
    expect(formatLatency(0.42)).toBe("0.4 ms");
    expect(formatLatency(23.6)).toBe("24 ms");
  });
});

describe("replicas", () => {
  it("allows 1–16, one with a volume", () => {
    expect(replicasError("1", false)).toBeNull();
    expect(replicasError("3", false)).toBeNull();
    expect(replicasError("1", true)).toBeNull();
    expect(replicasError("2", true)).toMatch(/volume/);
    expect(replicasError("0", false)).toMatch(/Between/);
    expect(replicasError("17", false)).toMatch(/Between/);
    expect(replicasError("1.5", false)).toMatch(/Between/);
  });
});
