import { describe, expect, it } from "vitest";
import { domainStatus, parsePort, parseRateLimit } from "./domains";

describe("domains", () => {
  it("labels verification states", () => {
    expect(domainStatus({ kind: "generated", status: "verified" })).toBeNull();
    expect(domainStatus({ kind: "custom", status: "pending" })?.label).toBe("Waiting for DNS");
    expect(domainStatus({ kind: "custom", status: "verified" })?.tone).toBe("success");
    expect(domainStatus({ kind: "custom", status: "failed" })?.tone).toBe("danger");
  });

  it("parses ports", () => {
    expect(parsePort("")).toEqual({ port: null });
    expect(parsePort(" 3000 ")).toEqual({ port: 3000 });
    expect(parsePort("0")).toHaveProperty("error");
    expect(parsePort("70000")).toHaveProperty("error");
    expect(parsePort("80a")).toHaveProperty("error");
  });

  it("parses rate limits", () => {
    expect(parseRateLimit("")).toEqual({ limit: null });
    expect(parseRateLimit("600")).toEqual({ limit: 600 });
    expect(parseRateLimit("-1")).toHaveProperty("error");
  });
});
