import { describe, expect, it } from "vitest";
import {
  approxUsd,
  defaultLocation,
  hasRootDisk,
  isApproximate,
  keyRegion,
  monthlyUsd,
  parseDiskGb,
  priceWithDisk,
  providerLabel,
  providerName,
  providerOf,
  stepIndex,
  typesIn,
  usd,
  validServerName,
  type ProviderLocation,
  type ProvisionedList,
  type ServerTypeOffer,
} from "./provision";

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

const place = (id: string): ProviderLocation => ({
  region: id.split(":")[1]!,
  location: { id, label: id, provider: id.split(":")[0]!, countryCode: "DE", city: id },
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
    expect(usd(452)).toBe("$4.52");
    expect(approxUsd(452)).toBe("~$4.52");
    expect(stepIndex("joining")).toBe(2);
    expect(stepIndex("failed")).toBe(-1);
  });
});

describe("provider kinds", () => {
  it("names them, and shows an unknown kind as it is", () => {
    expect(providerLabel("hetzner")).toBe("Hetzner Cloud");
    expect(providerLabel("aws")).toBe("AWS EC2");
    expect(providerName("aws")).toBe("AWS");
    expect(providerLabel("scaleway")).toBe("scaleway");
    expect(providerName(null)).toBe("its provider");
  });

  it("marks only Hetzner's prices as approximate", () => {
    expect(isApproximate("hetzner")).toBe(true);
    expect(isApproximate("aws")).toBe(false);
    expect(monthlyUsd(452, "hetzner")).toBe("~$4.52");
    expect(monthlyUsd(1240, "aws")).toBe("$12.40");
    expect(monthlyUsd(1240, null)).toBe("$12.40");
  });

  it("finds a provider in a server list", () => {
    const caps = { maxServers: 5, maxMonthlyCents: 5000 };
    const usage = { servers: 1, monthlyCents: 452 };
    const list: ProvisionedList = {
      servers: [],
      connectedProviders: [{ kind: "aws", caps, usage }],
    };
    expect(providerOf(list, "aws")?.usage.servers).toBe(1);
    expect(providerOf(list, "hetzner")).toBeUndefined();
    expect(providerOf(undefined, "aws")).toBeUndefined();
  });

  it("picks the usual first location when it is offered, else the first listed", () => {
    const hz = [place("hetzner:hel1"), place("hetzner:fsn1")];
    expect(defaultLocation("hetzner", hz)).toBe("hetzner:fsn1");
    expect(defaultLocation("aws", [place("aws:af-south-1"), place("aws:us-east-1")])).toBe("aws:us-east-1");
    expect(defaultLocation("aws", [place("aws:eu-west-1"), place("aws:af-south-1")])).toBe("aws:eu-west-1");
    expect(defaultLocation("aws", [])).toBeNull();
  });
});

describe("EC2 root disk", () => {
  it("prices a disk against the 80 GB the catalog assumes, 8 cents a GB", () => {
    // Mirrors crates/ec2 `disk_changes_the_price` and `disk_delta_cents`.
    expect(priceWithDisk("aws", 1000, 80)).toBe(1000);
    expect(priceWithDisk("aws", 1000, 100)).toBe(1160);
    expect(priceWithDisk("aws", 1000, 180)).toBe(1800);
    expect(priceWithDisk("aws", 1000, 40)).toBe(680);
  });

  it("leaves other providers' prices alone: the disk comes with the type", () => {
    expect(hasRootDisk("aws")).toBe(true);
    expect(hasRootDisk("hetzner")).toBe(false);
    expect(priceWithDisk("hetzner", 452, 500)).toBe(452);
  });

  it("accepts whole GB from 20 to 16384", () => {
    expect(parseDiskGb("80")).toBe(80);
    expect(parseDiskGb(" 20 ")).toBe(20);
    expect(parseDiskGb("16384")).toBe(16384);
    expect(parseDiskGb("19")).toBeNull();
    expect(parseDiskGb("16385")).toBeNull();
    expect(parseDiskGb("")).toBeNull();
    expect(parseDiskGb("80.5")).toBeNull();
    expect(parseDiskGb("-80")).toBeNull();
    expect(parseDiskGb("1e3")).toBeNull();
  });

  it("reads the region from a key id", () => {
    expect(keyRegion("eu-west-1/laptop")).toBe("eu-west-1");
    expect(keyRegion("eu-west-1/my/key")).toBe("eu-west-1");
  });
});
