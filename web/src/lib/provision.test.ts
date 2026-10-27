import { describe, expect, it } from "vitest";
import { euros, stepIndex, typesIn, validServerName, type ServerTypeOffer } from "./provision";

const offer = (name: string, cores: number, prices: Record<string, number>): ServerTypeOffer => ({
  name,
  description: name,
  cores,
  memoryGb: cores * 2,
  diskGb: 40,
  arch: "x86",
  cpuType: "shared",
  prices: Object.entries(prices).map(([location, monthlyCents]) => ({
    location,
    monthlyCents,
  })),
});

describe("provisioning helpers", () => {
  it("lists types orderable in a location, cheapest first", () => {
    const types = [offer("cx32", 4, { "hetzner:fsn1": 821 }), offer("cx22", 2, { "hetzner:fsn1": 452, "hetzner:hel1": 452 })];
    expect(typesIn(types, "hetzner:fsn1").map((t) => [t.name, t.monthlyCents])).toEqual([
      ["cx22", 452],
      ["cx32", 821],
    ]);
    expect(typesIn(types, "hetzner:hel1").map((t) => t.name)).toEqual(["cx22"]);
    expect(typesIn(types, "hetzner:ash")).toEqual([]);
  });

  it("validates names like the server", () => {
    expect(validServerName("worker-1")).toBe(true);
    expect(validServerName("Worker")).toBe(false);
    expect(validServerName("-a")).toBe(false);
    expect(validServerName("a".repeat(64))).toBe(false);
  });

  it("formats prices and steps", () => {
    expect(euros(452)).toBe("€4.52");
    expect(stepIndex("joining")).toBe(2);
    expect(stepIndex("failed")).toBe(-1);
  });
});
