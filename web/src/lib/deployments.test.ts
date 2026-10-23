import { describe, expect, it } from "vitest";
import { deploymentTitle, viaLabel } from "./deployments";

const base = { trigger: "manual", sourceKind: "github" as const, commitSha: "abc1234def", image: "x:1", commitMessage: null };

describe("deployments", () => {
  it("labels the source", () => {
    expect(viaLabel(base)).toBe("via GitHub");
    expect(viaLabel({ ...base, sourceKind: "image" })).toBe("via Docker image");
    expect(viaLabel({ ...base, sourceKind: "upload" })).toBe("via Upload");
    expect(viaLabel({ ...base, trigger: "rollback" })).toBe("via Rollback");
    expect(viaLabel({ ...base, trigger: "redeploy" })).toBe("via Redeploy");
  });
  it("falls back when sourceKind is missing", () => {
    expect(viaLabel({ ...base, sourceKind: undefined, commitSha: null })).toBe("via Docker image");
  });
  it("titles by first commit line", () => {
    expect(deploymentTitle({ ...base, commitMessage: "fix: a\n\nbody" })).toBe("fix: a");
    expect(deploymentTitle({ ...base, commitSha: null })).toBe("x:1");
  });
});
