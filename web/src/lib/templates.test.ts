import { describe, expect, it } from "vitest";
import { endpointHint } from "./backup";
import { formatBytes } from "./format";
import { r2Choice } from "@/pages/project/BucketFields";
import {
  isDatabaseTemplate,
  serviceName,
  slugifyServiceName,
  suggestBucketName,
  templateStack,
  uniqueName,
  validBucketName,
  validServiceName,
} from "./templates";

describe("service names", () => {
  it("are made from free text as you type", () => {
    expect(slugifyServiceName("Postgres API")).toBe("postgres-api");
    expect(slugifyServiceName("@acme/web")).toBe("acme-web");
    expect(slugifyServiceName("2nd  worker")).toBe("nd-worker");
    // Typing on past a separator works: the trailing `-` waits for more.
    expect(slugifyServiceName("postgres ")).toBe("postgres-");
    expect(slugifyServiceName("x".repeat(80))).toHaveLength(63);
  });
  it("drop a trailing separator when used", () => {
    expect(serviceName("Postgres API ")).toBe("postgres-api");
    expect(serviceName("!!")).toBe("");
  });
  it("follow the server's rule", () => {
    expect(validServiceName("postgres-api")).toBe(true);
    for (const bad of ["", "Postgres", "-db", "db-", "1db", "shared", "a".repeat(64)]) expect(validServiceName(bad)).toBe(false);
    expect(validServiceName(serviceName("Postgres API"))).toBe(true);
  });
});

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

describe("buckets", () => {
  it("suggests valid R2 names from service and environment", () => {
    expect(suggestBucketName("media", "production")).toBe("media-production");
    expect(suggestBucketName("My_Files", "Staging 2")).toBe("my-files-staging-2");
    expect(suggestBucketName("x", undefined)).toBe("bucket");
    const long = suggestBucketName("a".repeat(60), "production");
    expect(long.length).toBeLessThanOrEqual(63);
    expect(validBucketName(long)).toBe(true);
  });
  it("validates R2 names", () => {
    expect(validBucketName("abc")).toBe(true);
    expect(validBucketName("ab")).toBe(false);
    expect(validBucketName("-abc")).toBe(false);
    expect(validBucketName("a_b_c")).toBe(false);
  });
  it("builds the create-or-link choice", () => {
    expect(r2Choice("new", " media ", "")).toEqual({ bucket: "media" });
    expect(r2Choice("new", "Bad Name", "")).toBeNull();
    expect(r2Choice("existing", "media", "")).toBeNull();
    expect(r2Choice("existing", "", "old")).toEqual({ existing: "old" });
  });
  it("keeps buckets out of the database templates", () => {
    expect(isDatabaseTemplate("postgres")).toBe(true);
    expect(isDatabaseTemplate("bucket")).toBe(false);
    expect(templateStack("r2")).toBe("bucket");
  });
});
