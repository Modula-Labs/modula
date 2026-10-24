import { describe, expect, it } from "vitest";
import { deliveryStatus, isWebhookUrl } from "./webhooks";

describe("isWebhookUrl", () => {
  it.each([
    ["https://example.com/hook", true],
    [" http://localhost:3000/x ", true],
    ["ftp://example.com", false],
    ["example.com", false],
    ["", false],
  ])("%s -> %s", (url, ok) => expect(isWebhookUrl(url)).toBe(ok));
});

describe("deliveryStatus", () => {
  it("is neutral before the first delivery", () =>
    expect(deliveryStatus({ lastStatus: null, lastError: null, lastAt: null })).toEqual({ label: "Never sent", tone: "neutral" }));
  it("is a success for 2xx", () =>
    expect(deliveryStatus({ lastStatus: 204, lastError: null, lastAt: "2026-01-01T00:00:00Z" })).toEqual({ label: "204 OK", tone: "success" }));
  it("shows other statuses as failures", () =>
    expect(deliveryStatus({ lastStatus: 500, lastError: null, lastAt: "2026-01-01T00:00:00Z" }).tone).toBe("danger"));
  it("shows the error when there was no response", () =>
    expect(deliveryStatus({ lastStatus: null, lastError: "timeout", lastAt: "2026-01-01T00:00:00Z" })).toEqual({ label: "timeout", tone: "danger" }));
});
