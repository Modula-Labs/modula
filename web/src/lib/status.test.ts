import { describe, expect, it } from "vitest";
import { badgeOf, deploymentBadge, isLive, isServingStatus, serviceState } from "./status";

describe("sleeping status", () => {
  it("reads as a neutral, serving state", () => {
    expect(deploymentBadge.sleeping).toEqual({ label: "Sleeping", tone: "neutral" });
    expect(serviceState("sleeping")).toEqual({ label: "Sleeping", tone: "neutral" });
    expect(isServingStatus("sleeping")).toBe(true);
    expect(isLive("sleeping")).toBe(false);
  });
  it("keeps the other serving states", () => {
    expect(isServingStatus("active")).toBe(true);
    expect(isServingStatus("crashed")).toBe(true);
    expect(isServingStatus("failed")).toBe(false);
    expect(isServingStatus("removed")).toBe(false);
  });
});

describe("degraded deployments", () => {
  it("reads as Degraded only while active", () => {
    expect(badgeOf({ status: "active", degraded: true })).toEqual({ label: "Degraded", tone: "warn" });
    expect(badgeOf({ status: "active", degraded: false }).label).toBe("Active");
    expect(badgeOf({ status: "crashed", degraded: true }).label).toBe("Crashed");
    expect(badgeOf({ status: "sleeping" }).label).toBe("Sleeping");
    expect(serviceState("active", true)).toEqual({ label: "Degraded", tone: "warn" });
    expect(serviceState("active").label).toBe("Online");
  });
});
