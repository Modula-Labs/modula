import { describe, expect, it } from "vitest";
import { endpointHint } from "./backup";
import { formatBytes } from "./format";
import { templateStack, uniqueName } from "./templates";

describe("uniqueName", () => {
  it("keeps a free name", () => expect(uniqueName("postgres", ["web"])).toBe("postgres"));
  it("numbers from 2", () => expect(uniqueName("postgres", ["postgres"])).toBe("postgres-2"));
  it("skips taken numbers, ignoring case", () => expect(uniqueName("redis", ["Redis", "redis-2"])).toBe("redis-3"));
});

describe("templateStack", () => {
  it("maps mongo to the mongodb icon", () => {
    expect(templateStack("mongo")).toBe("mongodb");
    expect(templateStack("postgres")).toBe("postgres");
    expect(templateStack(null)).toBeNull();
  });
});

describe("formatBytes", () => {
  it("formats", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(1536)).toBe("1.5 KB");
    expect(formatBytes(10 * 1024 * 1024)).toBe("10 MB");
    expect(formatBytes(null)).toBe("—");
  });
});

describe("endpointHint", () => {
  it("builds per provider", () => {
    expect(endpointHint("r2")).toBe("https://<account>.r2.cloudflarestorage.com");
    expect(endpointHint("s3", "eu-west-1")).toBe("https://s3.eu-west-1.amazonaws.com");
    expect(endpointHint("b2")).toBe("https://s3.<region>.backblazeb2.com");
  });
});
