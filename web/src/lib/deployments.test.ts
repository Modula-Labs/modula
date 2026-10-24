import { describe, expect, it } from "vitest";
import { deploymentTitle, sleepingLine, viaLabel } from "./deployments";

const base = { trigger: "manual", sourceKind: "github" as const, commitSha: "abc1234def", image: "x:1", commitMessage: null };

describe("deployments", () => {
  it("labels the source", () => {
    expect(viaLabel(base)).toBe("via GitHub");
    expect(viaLabel({ ...base, sourceKind: "image" })).toBe("via Docker image");
    expect(viaLabel({ ...base, sourceKind: "upload" })).toBe("Uploaded via CLI from abc1234");
    expect(viaLabel({ ...base, sourceKind: "upload", sourceBranch: "main", sourceDirty: true })).toBe(
      "Uploaded via CLI from main@abc1234 with uncommitted changes",
    );
    expect(viaLabel({ ...base, sourceKind: "upload", commitSha: null })).toBe("Uploaded via CLI");
    expect(viaLabel({ ...base, trigger: "rollback" })).toBe("via Rollback");
    expect(viaLabel({ ...base, trigger: "redeploy" })).toBe("via Redeploy");
  });
  it("falls back when sourceKind is missing", () => {
    expect(viaLabel({ ...base, sourceKind: undefined, commitSha: null })).toBe("via Docker image");
  });
  it("titles by first commit line", () => {
    expect(deploymentTitle({ ...base, commitMessage: "fix: a\n\nbody" })).toBe("fix: a");
    expect(deploymentTitle({ ...base, commitSha: null })).toBe("x:1");
    expect(deploymentTitle({ ...base, commitSha: null, sourceKind: "upload" })).toBe("Local files");
  });
  it("describes a sleeping deployment", () => {
    const now = Date.parse("2026-01-01T12:00:00Z");
    expect(sleepingLine({ wokeAt: null, coldStartMs: null }, now)).toBe("Sleeping · wakes on request");
    expect(sleepingLine({ wokeAt: "2026-01-01T11:57:00Z", coldStartMs: 412 }, now)).toBe(
      "Sleeping · wakes on request · Last cold start 412 ms · 3m ago",
    );
    expect(sleepingLine({ wokeAt: null, coldStartMs: 1500 }, now)).toBe("Sleeping · wakes on request · Last cold start 1.5 s");
  });
});
