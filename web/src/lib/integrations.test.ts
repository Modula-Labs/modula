import { describe, expect, it } from "vitest";
import { emailDomain, emailReadinessMessage, sslModeLabel, zoneFor } from "./integrations";

describe("cloudflare helpers", () => {
  it("finds the deepest zone for a hostname", () => {
    const zones = ["example.com", "eu.example.com", "shop.dev"];
    expect(zoneFor("www.example.com", zones)).toBe("example.com");
    expect(zoneFor("api.eu.example.com", zones)).toBe("eu.example.com");
    expect(zoneFor("*.shop.dev", zones)).toBe("shop.dev");
    expect(zoneFor("Example.com.", zones)).toBe("example.com");
    expect(zoneFor("notexample.com", zones)).toBeNull();
  });
  it("labels SSL modes", () => {
    expect(sslModeLabel("strict")).toBe("Full (strict)");
    expect(sslModeLabel(null)).toBe("Unknown");
  });
});

describe("email sender", () => {
  it("takes the domain from the sender", () => {
    expect(emailDomain("Alerts@Example.com")).toBe("example.com");
    expect(emailDomain("example.com")).toBe("example.com");
  });
  it("says what is missing", () => {
    const at = (state: "ready" | "no_zone" | "missing_permission" | "not_onboarded") =>
      emailReadinessMessage({ from: "alerts@example.com", readiness: { state } });
    expect(at("ready")).toEqual({ text: "Ready to send", tone: "success" });
    expect(at("missing_permission")).toEqual({ text: "Add Email Sending · Edit to the token", tone: "warn" });
    expect(at("not_onboarded")).toEqual({
      text: "Onboard example.com under Email Service → Email Sending in Cloudflare",
      tone: "warn",
    });
    expect(at("no_zone")).toEqual({ text: "example.com isn't a zone this token can reach", tone: "warn" });
  });
  it("has nothing to say without a sender", () => {
    expect(emailReadinessMessage({ from: null, readiness: null })).toBeNull();
    expect(emailReadinessMessage({ from: "a@b.co", readiness: null })).toBeNull();
  });
});
