import { describe, expect, it } from "vitest";
import { resolveStackIcon } from "./stack";

describe("resolveStackIcon", () => {
  it("prefers a known explicit stack over image and repo", () => {
    expect(resolveStackIcon({ stack: "nextjs", image: "postgres:16", repo: "a/b" })).toBe("nextjs");
  });
  it("ignores unknown stack ids and falls through", () => {
    expect(resolveStackIcon({ stack: "cobol", repo: "a/b" })).toBe("github");
  });
  it.each([
    ["postgres:16", "postgres"],
    ["bitnami/postgresql:15", "postgres"],
    ["docker.io/library/redis", "redis"],
    ["valkey/valkey:8", "valkey"],
    ["ghcr.io/x/clickhouse-server:24.3", "clickhouse"],
    ["mongo:7", "mongodb"],
    ["mariadb:11", "mariadb"],
    ["minio/minio:latest", "minio"],
    ["getmeili/meilisearch:v1", "meilisearch"],
    ["node:22-alpine", "nodejs"],
    ["registry.example.com:5000/acme/app:1", "docker"],
  ])("image %s -> %s", (image, id) => {
    expect(resolveStackIcon({ image })).toBe(id);
  });
  it("uses github for repos, generic for nothing", () => {
    expect(resolveStackIcon({ repo: "a/b" })).toBe("github");
    expect(resolveStackIcon({})).toBe("generic");
  });
});
