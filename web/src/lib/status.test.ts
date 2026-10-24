import { describe, expect, it } from "vitest";
import { deploymentBadge, isLive, isServingStatus, serviceState } from "./status";

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
