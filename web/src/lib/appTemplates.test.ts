import { describe, expect, it } from "vitest";
import type { AppTemplate, Template } from "./api";
import { appMonogram, describeAppService, filterAppTemplates, serviceCount } from "./appTemplates";

const app = (id: string, name: string, category: string, roles: string[]): AppTemplate => ({
  id,
  name,
  description: `${name} description`,
  category,
  docsUrl: `https://example.com/${id}`,
  services: roles.map((role) => ({
    role,
    template: role === "postgres" ? "postgres" : null,
    image: role === "postgres" ? "postgres:17" : `ghcr.io/acme/${role}:1.2@sha256:abc`,
    mountPath: null,
    domain: role === id,
  })),
});

const apps = [
  app("n8n", "n8n", "Automation", ["postgres", "n8n"]),
  app("plausible", "Plausible Analytics", "Analytics", ["postgres", "clickhouse", "plausible"]),
  app("uptime-kuma", "Uptime Kuma", "Monitoring", ["uptime-kuma"]),
];

describe("filterAppTemplates", () => {
  it("returns everything for an empty query", () => {
    expect(filterAppTemplates(apps, "  ")).toBe(apps);
  });
  it("matches every word across name, category and services, ignoring case", () => {
    expect(filterAppTemplates(apps, "ANALYTICS").map((a) => a.id)).toEqual(["plausible"]);
    expect(filterAppTemplates(apps, "postgres").map((a) => a.id)).toEqual(["n8n", "plausible"]);
    expect(filterAppTemplates(apps, "clickhouse analytics").map((a) => a.id)).toEqual(["plausible"]);
    expect(filterAppTemplates(apps, "kuma automation")).toEqual([]);
  });
});

describe("serviceCount", () => {
  it("pluralises", () => {
    expect(serviceCount(apps[0]!)).toBe("2 services");
    expect(serviceCount(apps[2]!)).toBe("1 service");
  });
});

describe("appMonogram", () => {
  it("takes up to two initials", () => {
    expect(appMonogram("Uptime Kuma")).toBe("UK");
    expect(appMonogram("n8n")).toBe("N");
    expect(appMonogram("Plausible Analytics (CE)")).toBe("PA");
  });
});

describe("describeAppService", () => {
  const databases = [{ id: "postgres", name: "PostgreSQL" }] as Template[];
  it("names databases by their template and images by name and tag", () => {
    expect(describeAppService(apps[0]!.services[0]!, databases)).toBe("PostgreSQL database");
    expect(describeAppService(apps[0]!.services[0]!)).toBe("postgres database");
    expect(describeAppService(apps[0]!.services[1]!)).toBe("n8n 1.2");
    expect(describeAppService({ ...apps[0]!.services[1]!, image: "nginx" })).toBe("nginx");
  });
});
