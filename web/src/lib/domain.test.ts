import { describe, expect, it } from "vitest";
import { dashboardDomain, dnsRecords, hostError, normalizeHost } from "./domain";

describe("instance domain helpers", () => {
  it("normalizes pasted values", () => {
    expect(normalizeHost(" https://Deploy.Example.com:8443/path?x=1 ")).toBe("deploy.example.com");
    expect(normalizeHost("apps.example.com.")).toBe("apps.example.com");
    expect(normalizeHost("")).toBe("");
  });
  it("validates hostnames", () => {
    expect(hostError("")).toBeNull();
    expect(hostError("deploy.example.com")).toBeNull();
    expect(hostError("localhost")).not.toBeNull();
    expect(hostError("1.2.3.4")).not.toBeNull();
    expect(hostError("bad_name.example.com")).not.toBeNull();
    expect(hostError("-a.example.com")).not.toBeNull();
  });
  it("computes DNS records", () => {
    expect(dnsRecords("deploy.example.com", "apps.example.com", "1.2.3.4")).toEqual([
      { type: "A", name: "deploy.example.com", value: "1.2.3.4", purpose: "dashboard" },
      { type: "A", name: "*.apps.example.com", value: "1.2.3.4", purpose: "apps" },
    ]);
    expect(dnsRecords("", "apps.example.com", "1.2.3.4")).toHaveLength(1);
    expect(dnsRecords("a.example.com", "", null)).toEqual([]);
  });
  it("treats a domain install address as the dashboard domain", () => {
    expect(dashboardDomain({ dashboard: null, installUrl: "https://deploy.example.com" })).toBe("deploy.example.com");
    expect(dashboardDomain({ dashboard: null, installUrl: "https://203.0.113.7" })).toBeNull();
    expect(dashboardDomain({ dashboard: "d.example.org", installUrl: "https://203.0.113.7" })).toBe("d.example.org");
  });
});
