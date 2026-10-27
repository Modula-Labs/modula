import { describe, expect, it } from "vitest";
import { sslModeLabel, zoneFor } from "./integrations";

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
